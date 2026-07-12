mod args;
mod tool_discovery;

use centaur_api_server::{AppState, build_router_with_app_state};
use centaur_session_runtime::SessionRuntime;
use centaur_session_sqlx::PgSessionStore;
use centaur_telemetry::{TelemetryConfig, init_telemetry};
use centaur_workflows::{WorkflowRuntime, require_rollback_bridge_workflow_pause};
use clap::Parser;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::info;

use args::Args;

const CONTROL_API_KEY_ENV: &str = "CENTAUR_CONTROL_API_KEY";
const SERVICE_API_KEY_ENVS: [&str; 8] = [
    CONTROL_API_KEY_ENV,
    "SLACKBOT_API_KEY",
    "GITHUBBOT_API_KEY",
    "LINEARBOT_API_KEY",
    "DISCORDBOT_API_KEY",
    "TEAMSBOT_API_KEY",
    "WORKFLOW_API_KEY",
    "SLACK_FEEDBACK_API_KEY",
];

#[tokio::main]
async fn main() -> Result<(), ServerError> {
    init_crypto_provider();
    // Fail before binding a listener or touching the database if the rollback
    // deployment omitted its mandatory durable-workflow preservation fence.
    require_rollback_bridge_workflow_pause()?;
    require_distinct_service_api_keys()?;
    let args = Args::parse();
    require_rollback_bridge_migrations_disabled(args.server.run_migrations)?;
    let telemetry = init_telemetry(TelemetryConfig::from_env())?;
    let listener = TcpListener::bind(args.server.bind_addr).await?;
    info!(
        bind_addr = %args.server.bind_addr,
        "starting centaur api-rs server"
    );

    let app_state = AppState::unready();
    let app = build_router_with_app_state(app_state.clone());
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    });

    tokio::select! {
        result = &mut server => {
            result??;
            telemetry.shutdown();
            return Ok(());
        }
        result = initialize_runtime(args, app_state) => {
            if let Err(error) = result {
                server.abort();
                telemetry.shutdown();
                return Err(error);
            }
        }
    }

    server.await??;
    telemetry.shutdown();
    Ok(())
}

fn require_distinct_service_api_keys() -> Result<(), ServerError> {
    let configured = SERVICE_API_KEY_ENVS
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (*name, value)))
        .collect::<Vec<_>>();
    validate_service_api_key_separation(
        configured
            .iter()
            .map(|(name, value)| (*name, value.as_str())),
    )
    .map_err(ServerError::UnsupportedConfig)
}

fn require_rollback_bridge_migrations_disabled(run_migrations: bool) -> Result<(), ServerError> {
    if run_migrations {
        return Err(ServerError::UnsupportedConfig(
            "rollback bridge refuses to start with RUN_MIGRATIONS=true; the reviewed forward runtime owns schema migration"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_service_api_key_separation<'a>(
    configured: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), String> {
    use std::collections::BTreeMap;

    let configured = configured
        .into_iter()
        .map(|(name, value)| (name, value.trim()))
        .filter(|(_, value)| !value.is_empty())
        .collect::<Vec<_>>();
    if !configured
        .iter()
        .any(|(name, _)| *name == CONTROL_API_KEY_ENV)
    {
        return Err(format!(
            "rollback bridge refuses to start unless {CONTROL_API_KEY_ENV} is configured"
        ));
    }

    let mut owners = BTreeMap::new();
    for (name, value) in configured {
        if let Some(existing) = owners.insert(value, name) {
            return Err(format!(
                "rollback bridge refuses to start because {existing} and {name} must contain distinct service credentials"
            ));
        }
    }
    Ok(())
}

async fn initialize_runtime(args: Args, app_state: AppState) -> Result<(), ServerError> {
    let store = PgSessionStore::connect(&args.server.database_url).await?;
    let pool = store.pool().clone();
    let sandbox_runtime = args.sandbox_runtime().await?;
    let mut runtime = SessionRuntime::new(store.clone(), sandbox_runtime);
    let mut warm_pool_bootstrap_principal = None;
    let mut workflow_host_principal = None;
    if let Some(iron_control) = args.iron_control_runtime().await? {
        info!("iron-control session registration enabled");
        warm_pool_bootstrap_principal = Some(iron_control.warm_pool_bootstrap_principal);
        workflow_host_principal = Some(iron_control.workflow_host_principal);
        runtime = runtime.with_iron_control(iron_control.registrar);
    }
    if let Some(reconciler) = args.iron_control_tool_reconciler()? {
        info!("iron-control tool secret reconciliation enabled");
        tokio::spawn(reconciler.run());
    }
    runtime = runtime.with_personas(args.persona_registry()?);
    if let Some(mut config) = args.warm_pool_config() {
        config.bootstrap_iron_control_principal = warm_pool_bootstrap_principal.clone();
        runtime = runtime.with_warm_pool(config);
    }
    runtime = runtime.with_sandbox_reaper(args.sandbox_reaper_config());
    runtime = runtime.with_sandbox_cleanup(args.sandbox_cleanup_config());
    let workflow_host_sandbox = args
        .workflow_host_sandbox_runtime(workflow_host_principal.as_deref())
        .await?;
    let workflows = Some(
        WorkflowRuntime::new_with_workflow_host_sandbox(
            store,
            runtime.clone(),
            workflow_host_sandbox,
        )
        .await?,
    );

    // Adopt executions orphaned by the previous process (deploy/crash):
    // recover finished turns from recorded sandbox output, re-attach still
    // running sandboxes, and fail the rest so their threads unwedge.
    let adoption_runtime = runtime.clone();
    tokio::spawn(async move {
        adoption_runtime.adopt_orphaned_executions().await;
    });

    app_state.mark_ready(runtime, workflows, Some(pool));
    info!("centaur api-rs runtime initialized");
    Ok(())
}

fn init_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[derive(Debug, Error)]
pub(crate) enum ServerError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Store(#[from] centaur_session_sqlx::SessionStoreError),
    #[error(transparent)]
    Workflows(#[from] centaur_workflows::WorkflowRuntimeError),
    #[error(transparent)]
    KubeConfig(#[from] kube::config::KubeconfigError),
    #[error(transparent)]
    KubeInferConfig(#[from] kube::config::InferConfigError),
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error(transparent)]
    IronProxy(#[from] centaur_iron_proxy::IronProxyConfigError),
    #[error(transparent)]
    IronControl(#[from] centaur_iron_control::IronControlError),
    #[error(transparent)]
    IronControlRegister(#[from] centaur_iron_control::RegisterError),
    #[error(transparent)]
    Telemetry(#[from] centaur_telemetry::TelemetryError),
    #[error(transparent)]
    ToolDiscovery(#[from] tool_discovery::ToolDiscoveryError),
    #[error("tool source error: {0}")]
    ToolSource(String),
    #[error("iron-proxy requires both firewall CA cert and key Secret names")]
    MissingIronProxyCaSecret,
    #[error("{0}")]
    UnsupportedConfig(String),
}

#[cfg(test)]
mod rollback_bridge_config_tests {
    use super::*;

    #[test]
    fn control_key_is_required_and_every_service_key_is_pairwise_distinct() {
        assert!(validate_service_api_key_separation([]).is_err());
        assert!(validate_service_api_key_separation([(CONTROL_API_KEY_ENV, "control")]).is_ok());
        for (index, left) in SERVICE_API_KEY_ENVS.iter().enumerate() {
            for right in &SERVICE_API_KEY_ENVS[index + 1..] {
                let error = validate_service_api_key_separation([
                    (CONTROL_API_KEY_ENV, "control"),
                    (left, "shared"),
                    (right, "shared"),
                ])
                .expect_err("reused service key must fail startup");
                assert!(error.contains(left), "unexpected error: {error}");
                assert!(error.contains(right), "unexpected error: {error}");
                assert!(!error.contains("shared"), "secret leaked in error: {error}");
            }
        }
        assert!(
            validate_service_api_key_separation([
                (CONTROL_API_KEY_ENV, "control"),
                ("SLACKBOT_API_KEY", "bot"),
                ("WORKFLOW_API_KEY", "workflow"),
                ("GITHUBBOT_API_KEY", ""),
            ])
            .is_ok()
        );
    }
}
