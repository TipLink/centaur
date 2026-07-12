use std::{
    collections::BTreeMap,
    env,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use centaur_session_core::{HarnessType, SandboxCapabilities, ThreadKey};
use centaur_session_sqlx::PgSessionStore;
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

const DATABASE_ENV: &str = "ROLLBACK_BRIDGE_FORWARD_TEST_DATABASE_URL";
const FORWARD_BINARY_ENV: &str = "ROLLBACK_BRIDGE_REHEARSAL_FORWARD_BIN";
const FORWARD_WORKFLOW_HOST_ENV: &str = "ROLLBACK_BRIDGE_REHEARSAL_FORWARD_WORKFLOW_HOST";
const FORWARD_WORKDIR_ENV: &str = "ROLLBACK_BRIDGE_REHEARSAL_FORWARD_WORKDIR";
const CONTROL_KEY: &str = "rollback-preservation-control-key";
const WORKFLOW_KEY: &str = "rollback-preservation-workflow-key";
const WEBHOOK_KEY: &str = "rollback-preservation-webhook-key";
const FORWARD_COMMIT_FILE: &str =
    include_str!("../../../../../.github/rollback-bridge-reviewed-forward-commit");
static DATABASE_REHEARSAL_LOCK: Mutex<()> = Mutex::const_new(());
const QUEUES: [&str; 5] = [
    "centaur_workflows",
    "centaur_workflows_slack_live",
    "centaur_workflows_etl",
    "centaur_workflows_etl_backfill",
    "centaur_workflow_schedules",
];
const FORWARD_MIGRATIONS: [(&str, &str); 11] = [
    (
        "0033_session_title.sql",
        include_str!("fixtures/forward_migrations/0033_session_title.sql"),
    ),
    (
        "0034_session_sandbox_activity.sql",
        include_str!("fixtures/forward_migrations/0034_session_sandbox_activity.sql"),
    ),
    (
        "0035_session_execution_stdout_owner.sql",
        include_str!("fixtures/forward_migrations/0035_session_execution_stdout_owner.sql"),
    ),
    (
        "0036_session_sandbox_api_server_capability.sql",
        include_str!("fixtures/forward_migrations/0036_session_sandbox_api_server_capability.sql"),
    ),
    (
        "0037_readonly_all_workflow_queues.sql",
        include_str!("fixtures/forward_migrations/0037_readonly_all_workflow_queues.sql"),
    ),
    (
        "0038_session_sandbox_repo_cache_access.sql",
        include_str!("fixtures/forward_migrations/0038_session_sandbox_repo_cache_access.sql"),
    ),
    (
        "0039_slack_private_channels.sql",
        include_str!("fixtures/forward_migrations/0039_slack_private_channels.sql"),
    ),
    (
        "0040_granola_sync_tables.sql",
        include_str!("fixtures/forward_migrations/0040_granola_sync_tables.sql"),
    ),
    (
        "0041_attio_sync_tables.sql",
        include_str!("fixtures/forward_migrations/0041_attio_sync_tables.sql"),
    ),
    (
        "0042_centaur_readonly_slack_dm_rls.sql",
        include_str!("fixtures/forward_migrations/0042_centaur_readonly_slack_dm_rls.sql"),
    ),
    (
        "0043_session_sandbox_content_revision.sql",
        include_str!("fixtures/forward_migrations/0043_session_sandbox_content_revision.sql"),
    ),
];
const EMBEDDED_FORWARD_MIGRATIONS: [(&str, &str); 11] = [
    (
        "0033_session_title.sql",
        include_str!("../../centaur-session-sqlx/migrations/0033_session_title.sql"),
    ),
    (
        "0034_session_sandbox_activity.sql",
        include_str!("../../centaur-session-sqlx/migrations/0034_session_sandbox_activity.sql"),
    ),
    (
        "0035_session_execution_stdout_owner.sql",
        include_str!(
            "../../centaur-session-sqlx/migrations/0035_session_execution_stdout_owner.sql"
        ),
    ),
    (
        "0036_session_sandbox_api_server_capability.sql",
        include_str!(
            "../../centaur-session-sqlx/migrations/0036_session_sandbox_api_server_capability.sql"
        ),
    ),
    (
        "0037_readonly_all_workflow_queues.sql",
        include_str!("../../centaur-session-sqlx/migrations/0037_readonly_all_workflow_queues.sql"),
    ),
    (
        "0038_session_sandbox_repo_cache_access.sql",
        include_str!(
            "../../centaur-session-sqlx/migrations/0038_session_sandbox_repo_cache_access.sql"
        ),
    ),
    (
        "0039_slack_private_channels.sql",
        include_str!("../../centaur-session-sqlx/migrations/0039_slack_private_channels.sql"),
    ),
    (
        "0040_granola_sync_tables.sql",
        include_str!("../../centaur-session-sqlx/migrations/0040_granola_sync_tables.sql"),
    ),
    (
        "0041_attio_sync_tables.sql",
        include_str!("../../centaur-session-sqlx/migrations/0041_attio_sync_tables.sql"),
    ),
    (
        "0042_centaur_readonly_slack_dm_rls.sql",
        include_str!(
            "../../centaur-session-sqlx/migrations/0042_centaur_readonly_slack_dm_rls.sql"
        ),
    ),
    (
        "0043_session_sandbox_content_revision.sql",
        include_str!(
            "../../centaur-session-sqlx/migrations/0043_session_sandbox_content_revision.sql"
        ),
    ),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_bridge_preserves_forward_schema_workflows_and_reassignment_fence() {
    let _database_guard = DATABASE_REHEARSAL_LOCK.lock().await;
    let Ok(admin_url) = env::var(DATABASE_ENV) else {
        eprintln!("skipping: {DATABASE_ENV} not set (required by rollback preservation CI)");
        return;
    };
    assert_fixture_provenance();

    let database_name = format!("rollback_bridge_{}", Uuid::new_v4().simple());
    let admin_pool = PgPool::connect(&admin_url)
        .await
        .expect("connect forward-test admin database");
    sqlx::query(&format!("create database {database_name}"))
        .execute(&admin_pool)
        .await
        .expect("create disposable forward database");
    let database_url = database_url_with_name(&admin_url, &database_name);

    let store = PgSessionStore::connect(&database_url)
        .await
        .expect("connect disposable forward database");
    store
        .run_migrations()
        .await
        .expect("apply reviewed forward migration ledger 0001-0043");
    let pool = store.pool().clone();
    let (migration_count, migration_max) =
        sqlx::query_as::<_, (i64, i64)>("select count(*), max(version) from _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .expect("read embedded migration ledger");
    assert_eq!((migration_count, migration_max), (43, 43));
    let migrated_queues = sqlx::query_scalar::<_, String>(
        "select queue_name from absurd.list_queues() order by queue_name",
    )
    .fetch_all(&pool)
    .await
    .expect("list forward migration-created queues");
    assert_eq!(migrated_queues.len(), QUEUES.len());
    assert!(
        QUEUES
            .iter()
            .all(|queue| migrated_queues.iter().any(|value| value == queue))
    );
    let seeded = seed_representative_workflow_rows(&pool).await;
    let reassigned = seed_forward_assignment_and_simulate_bridge_reassignment(&store, &pool).await;
    let before = canonical_forward_snapshot(&pool).await;
    assert_seeded_states(&pool).await;
    assert_reassignment_requires_forward_replacement(&pool, &reassigned).await;

    let port = unused_local_port();
    let workflow_dir = env::temp_dir().join(format!(
        "centaur-rollback-preservation-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&workflow_dir).expect("create workflow fixture directory");
    std::fs::write(
        workflow_dir.join("rollback_preservation_sentinel.py"),
        r#"WORKFLOW_NAME = "rollback_preservation_sentinel"

async def handler(params, ctx):
    return {"ok": True}
"#,
    )
    .expect("write workflow discovery sentinel");
    let mut server = BridgeProcess::spawn(&database_url, port, &workflow_dir);
    let client = Client::new();
    let base_url = format!("http://127.0.0.1:{port}");
    wait_for_ready(&client, &base_url, &mut server).await;

    exercise_read_and_mutation_lanes(&client, &base_url, &seeded.running_run_id).await;
    // This exceeds both test-configured one-second reconcile/reaper intervals.
    // Any accidentally started worker or metadata reaper has time to mutate
    // the pending, expired-running, sleeping, or forward-only handler rows.
    sleep(Duration::from_secs(3)).await;
    server.stop();

    let after = canonical_forward_snapshot(&pool).await;
    assert_eq!(
        after, before,
        "paused rollback bridge changed forward sessions, workflows, or migration ledger"
    );
    assert_seeded_states(&pool).await;
    assert_reassignment_requires_forward_replacement(&pool, &reassigned).await;

    pool.close().await;
    drop(store);
    sqlx::query(&format!("drop database {database_name} with (force)"))
        .execute(&admin_pool)
        .await
        .expect("drop disposable forward database");
    admin_pool.close().await;
    let _ = std::fs::remove_dir_all(workflow_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_bridge_missing_forward_queue_fails_without_schema_mutation() {
    let _database_guard = DATABASE_REHEARSAL_LOCK.lock().await;
    let Ok(admin_url) = env::var(DATABASE_ENV) else {
        eprintln!("skipping: {DATABASE_ENV} not set (required by rollback preservation CI)");
        return;
    };

    let database_name = format!("rollback_missing_queue_{}", Uuid::new_v4().simple());
    let admin_pool = PgPool::connect(&admin_url)
        .await
        .expect("connect missing-queue admin database");
    sqlx::query(&format!("create database {database_name}"))
        .execute(&admin_pool)
        .await
        .expect("create missing-queue database");
    let database_url = database_url_with_name(&admin_url, &database_name);
    let store = PgSessionStore::connect(&database_url)
        .await
        .expect("connect missing-queue database");
    store
        .run_migrations()
        .await
        .expect("apply embedded forward migration ledger");
    let pool = store.pool().clone();

    let migrated_queues = sqlx::query_scalar::<_, String>(
        "select queue_name from absurd.list_queues() order by queue_name",
    )
    .fetch_all(&pool)
    .await
    .expect("list migration-created workflow queues");
    assert_eq!(
        migrated_queues.len(),
        QUEUES.len(),
        "forward migrations must create all required queues: {migrated_queues:?}"
    );
    sqlx::query("select absurd.drop_queue($1)")
        .bind(QUEUES[4])
        .execute(&pool)
        .await
        .expect("remove schedule queue for negative startup fixture");
    let queues = sqlx::query_scalar::<_, String>(
        "select queue_name from absurd.list_queues() order by queue_name",
    )
    .fetch_all(&pool)
    .await
    .expect("list workflow queues after negative-fixture removal");
    assert_eq!(
        queues.len(),
        4,
        "unexpected migration-created queues: {queues:?}"
    );
    assert!(
        !queues.iter().any(|queue| queue == QUEUES[4]),
        "schedule queue must be absent in the negative fixture"
    );
    let before = rollback_schema_identity_snapshot(&pool).await;

    let workflow_dir = env::temp_dir().join(format!(
        "centaur-rollback-missing-queue-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&workflow_dir).expect("create missing-queue workflow directory");
    let port = unused_local_port();
    let mut command = bridge_command(&database_url, port, &workflow_dir);
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .expect("run bridge against missing forward queue");
    let client = Client::new();
    let ready_url = format!("http://127.0.0.1:{port}/readyz");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child
            .try_wait()
            .expect("inspect missing-queue bridge process")
            .is_some()
        {
            break;
        }
        if let Ok(response) = client.get(&ready_url).send().await
            && response.status() == StatusCode::OK
        {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect unexpectedly ready bridge output");
            panic!(
                "bridge became ready after creating the missing queue: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect timed-out missing-queue bridge output");
            panic!(
                "bridge did not fail missing-queue startup within 10 seconds: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        sleep(Duration::from_millis(100)).await;
    }
    let output = child
        .wait_with_output()
        .expect("collect missing-queue bridge output");
    assert!(!output.status.success(), "bridge unexpectedly became ready");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rollback bridge requires all forward absurd queues to exist")
            && stderr.contains(QUEUES[4]),
        "unexpected missing-queue startup failure: {stderr}"
    );

    let after = rollback_schema_identity_snapshot(&pool).await;
    assert_eq!(
        after, before,
        "bridge startup created or changed schema objects while rejecting a missing queue"
    );

    pool.close().await;
    drop(store);
    sqlx::query(&format!("drop database {database_name} with (force)"))
        .execute(&admin_pool)
        .await
        .expect("drop missing-queue database");
    admin_pool.close().await;
    let _ = std::fs::remove_dir_all(workflow_dir);
}

#[derive(Clone)]
struct RehearsalTask {
    queue: &'static str,
    task_id: Uuid,
    run_id: Uuid,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reviewed_forward_bridge_reviewed_forward_rehearsal() {
    let _database_guard = DATABASE_REHEARSAL_LOCK.lock().await;
    let Ok(admin_url) = env::var(DATABASE_ENV) else {
        eprintln!("skipping: {DATABASE_ENV} not set");
        return;
    };
    let Ok(forward_binary) = env::var(FORWARD_BINARY_ENV) else {
        eprintln!("skipping: {FORWARD_BINARY_ENV} not set (explicit cross-version CI sets it)");
        return;
    };
    let forward_workflow_host = env::var(FORWARD_WORKFLOW_HOST_ENV).unwrap_or_else(|_| {
        panic!("{FORWARD_WORKFLOW_HOST_ENV} is required with {FORWARD_BINARY_ENV}")
    });
    let forward_workdir = env::var(FORWARD_WORKDIR_ENV)
        .unwrap_or_else(|_| panic!("{FORWARD_WORKDIR_ENV} is required with {FORWARD_BINARY_ENV}"));
    assert_fixture_provenance();

    let database_name = format!("rollback_rehearsal_{}", Uuid::new_v4().simple());
    let admin_pool = PgPool::connect(&admin_url)
        .await
        .expect("connect cross-version admin database");
    sqlx::query(&format!("create database {database_name}"))
        .execute(&admin_pool)
        .await
        .expect("create cross-version database");
    let database_url = database_url_with_name(&admin_url, &database_name);
    let store = PgSessionStore::connect(&database_url)
        .await
        .expect("connect cross-version database");
    store
        .run_migrations()
        .await
        .expect("apply embedded reviewed forward migrations");
    let pool = store.pool().clone();

    let workflow_dir = env::temp_dir().join(format!(
        "centaur-cross-version-workflows-{}",
        Uuid::new_v4().simple()
    ));
    write_cross_version_workflows(&workflow_dir);
    let client = Client::new();

    // Phase 1: the pinned reviewed forward binary creates real durable tasks.
    let prepare_port = unused_local_port();
    let mut prepare = ForwardProcess::spawn(ForwardProcessConfig {
        binary: &forward_binary,
        workdir: &forward_workdir,
        workflow_host: &forward_workflow_host,
        database_url: &database_url,
        workflow_dir: &workflow_dir,
        port: prepare_port,
        phase: "prepare",
    });
    let prepare_url = format!("http://127.0.0.1:{prepare_port}");
    wait_for_forward_ready(&client, &prepare_url, &mut prepare).await;
    let running = create_forward_workflow_run(
        &client,
        &prepare_url,
        "rollback_running",
        "rehearsal-running",
        json!({"kind": "expired-claim", "payload": {"must": "survive"}}),
    )
    .await;
    wait_for_task_state(&pool, running.queue, running.task_id, "running").await;
    let pending = create_forward_workflow_run(
        &client,
        &prepare_url,
        "rollback_pending",
        "rehearsal-pending",
        json!({"kind": "pending", "payload": [1, 2, 3]}),
    )
    .await;
    let waiting = create_forward_workflow_run(
        &client,
        &prepare_url,
        "rollback_waiting",
        "rehearsal-waiting",
        json!({"kind": "await-event", "payload": {"opaque": true}}),
    )
    .await;
    wait_for_task_state(&pool, pending.queue, pending.task_id, "pending").await;
    wait_for_task_state(&pool, waiting.queue, waiting.task_id, "pending").await;
    let sleeping = create_forward_workflow_run(
        &client,
        &prepare_url,
        "slack_sync",
        "rehearsal-sleeping",
        json!({"kind": "sleeping", "cursor": 17}),
    )
    .await;
    wait_for_task_state(&pool, sleeping.queue, sleeping.task_id, "sleeping").await;
    prepare.stop();

    expire_running_claim(&pool, &running).await;
    convert_pending_to_event_wait(&pool, &waiting).await;
    customize_retry_and_cancellation_contract(&pool, [&running, &pending, &waiting, &sleeping])
        .await;
    let reassigned = seed_forward_assignment_and_simulate_bridge_reassignment(&store, &pool).await;
    let before_bridge = canonical_cross_version_snapshot(&pool).await;
    let immutable_task_contract =
        task_contract_snapshot(&pool, [&running, &pending, &waiting, &sleeping]).await;

    // Phase 2: the rollback bridge starts with workflow mutation hard-paused.
    let bridge_port = unused_local_port();
    let mut bridge = BridgeProcess::spawn(&database_url, bridge_port, &workflow_dir);
    let bridge_url = format!("http://127.0.0.1:{bridge_port}");
    wait_for_ready(&client, &bridge_url, &mut bridge).await;
    exercise_read_and_mutation_lanes(&client, &bridge_url, &running.run_id.to_string()).await;
    sleep(Duration::from_secs(3)).await;
    bridge.stop();
    let after_bridge = canonical_cross_version_snapshot(&pool).await;
    assert_eq!(
        after_bridge, before_bridge,
        "rollback bridge changed durable forward workflow/session state"
    );
    assert_reassignment_requires_forward_replacement(&pool, &reassigned).await;

    // Phase 3: the exact reviewed forward binary resumes every durable shape.
    let resume_port = unused_local_port();
    let mut resume = ForwardProcess::spawn(ForwardProcessConfig {
        binary: &forward_binary,
        workdir: &forward_workdir,
        workflow_host: &forward_workflow_host,
        database_url: &database_url,
        workflow_dir: &workflow_dir,
        port: resume_port,
        phase: "resume",
    });
    let resume_url = format!("http://127.0.0.1:{resume_port}");
    wait_for_forward_ready(&client, &resume_url, &mut resume).await;
    emit_forward_workflow_event(&client, &resume_url, "rollback.rehearsal.resume").await;
    execute_reassigned_session(&client, &resume_url, &reassigned).await;
    for task in [&running, &pending, &waiting, &sleeping] {
        wait_for_task_state(&pool, task.queue, task.task_id, "completed").await;
    }
    wait_for_forward_sandbox_replacement(&pool, &reassigned).await;
    let after_resume_contract =
        task_contract_snapshot(&pool, [&running, &pending, &waiting, &sleeping]).await;
    assert_eq!(
        after_resume_contract, immutable_task_contract,
        "re-forward changed task payload, retry, cancellation, or idempotency contract"
    );
    assert_checkpoint_survived_resume(&pool, &sleeping).await;
    resume.stop();

    pool.close().await;
    drop(store);
    sqlx::query(&format!("drop database {database_name} with (force)"))
        .execute(&admin_pool)
        .await
        .expect("drop cross-version database");
    admin_pool.close().await;
    let _ = std::fs::remove_dir_all(workflow_dir);
}

struct BridgeProcess {
    child: Option<Child>,
}

impl BridgeProcess {
    fn spawn(database_url: &str, port: u16, workflow_dir: &Path) -> Self {
        let mut command = bridge_command(database_url, port, workflow_dir);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        Self {
            child: Some(command.spawn().expect("start rollback bridge binary")),
        }
    }

    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child
            .as_mut()
            .expect("bridge child")
            .try_wait()
            .expect("inspect rollback bridge process")
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn bridge_command(database_url: &str, port: u16, workflow_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_centaur-api-server"));
    command
        .env("BIND_ADDR", format!("127.0.0.1:{port}"))
        .env("DATABASE_URL", database_url)
        .env("RUN_MIGRATIONS", "false")
        .env("CENTAUR_CONTROL_API_KEY", CONTROL_KEY)
        .env("CENTAUR_ROLLBACK_BRIDGE_PAUSE_WORKFLOWS", "true")
        .env("TRIVY_INTAKE_WEBHOOK_TOKEN", WEBHOOK_KEY)
        .env("WORKFLOW_DIRS", workflow_dir)
        .env("WORKFLOW_HOST_SANDBOX", "false")
        .env("WORKFLOW_REAP_REMOVED_AFTER_TICKS", "1")
        .env("WORKFLOW_RECONCILE_INTERVAL_SECS", "1")
        .env("RUST_LOG", "warn")
        .env_remove("SLACKBOT_API_KEY")
        .env_remove("GITHUBBOT_API_KEY")
        .env_remove("LINEARBOT_API_KEY")
        .env_remove("DISCORDBOT_API_KEY")
        .env_remove("TEAMSBOT_API_KEY")
        .env_remove("WORKFLOW_API_KEY")
        .env_remove("SLACK_FEEDBACK_API_KEY");
    command
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

struct ForwardProcess {
    child: Option<Child>,
}

struct ForwardProcessConfig<'a> {
    binary: &'a str,
    workdir: &'a str,
    workflow_host: &'a str,
    database_url: &'a str,
    workflow_dir: &'a Path,
    port: u16,
    phase: &'a str,
}

impl ForwardProcess {
    fn spawn(config: ForwardProcessConfig<'_>) -> Self {
        let mut command = Command::new(config.binary);
        command
            .current_dir(config.workdir)
            .env("BIND_ADDR", format!("127.0.0.1:{}", config.port))
            .env("DATABASE_URL", config.database_url)
            .env("RUN_MIGRATIONS", "false")
            .env("CENTAUR_CONTROL_API_KEY", CONTROL_KEY)
            .env("WORKFLOW_API_KEY", WORKFLOW_KEY)
            .env("WORKFLOW_DIRS", config.workflow_dir)
            .env("WORKFLOW_HOST_SANDBOX", "false")
            .env("PYTHON_WORKFLOW_HOST_PATH", config.workflow_host)
            .env("PYTHON_WORKFLOW_HOST_PYTHON", "python3")
            .env("WORKFLOW_WORKER_CONCURRENCY", "1")
            .env("WORKFLOW_ETL_WORKER_CONCURRENCY", "1")
            .env("WORKFLOW_REAP_REMOVED_AFTER_TICKS", "0")
            .env("WORKFLOW_RECONCILE_INTERVAL_SECS", "0")
            .env("SESSION_EXECUTION_ADOPTION_INTERVAL_SECS", "0")
            .env("ROLLBACK_REHEARSAL_PHASE", config.phase)
            .env("RUST_LOG", "warn")
            .env_remove("CENTAUR_ROLLBACK_BRIDGE_PAUSE_WORKFLOWS")
            .env_remove("SLACKBOT_API_KEY")
            .env_remove("GITHUBBOT_API_KEY")
            .env_remove("LINEARBOT_API_KEY")
            .env_remove("DISCORDBOT_API_KEY")
            .env_remove("TEAMSBOT_API_KEY")
            .env_remove("SLACK_FEEDBACK_API_KEY")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        Self {
            child: Some(command.spawn().expect("start reviewed forward binary")),
        }
    }

    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child
            .as_mut()
            .expect("forward child")
            .try_wait()
            .expect("inspect forward process")
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ForwardProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn write_cross_version_workflows(workflow_dir: &Path) {
    std::fs::create_dir_all(workflow_dir).expect("create cross-version workflow directory");
    let files = [
        (
            "rollback_running.py",
            r#"import asyncio
import os

WORKFLOW_NAME = "rollback_running"

async def handler(params, ctx):
    if os.environ.get("ROLLBACK_REHEARSAL_PHASE") == "prepare":
        await asyncio.sleep(300)
    return {"resumed": True, "input": params}
"#,
        ),
        (
            "rollback_pending.py",
            r#"WORKFLOW_NAME = "rollback_pending"

async def handler(params, ctx):
    return {"resumed": True, "input": params}
"#,
        ),
        (
            "rollback_waiting.py",
            r#"WORKFLOW_NAME = "rollback_waiting"

async def handler(params, ctx):
    return {"resumed": True, "input": params}
"#,
        ),
        (
            "slack_sync.py",
            r#"WORKFLOW_NAME = "slack_sync"

async def handler(params, ctx):
    checkpoint = await ctx.step(
        "rehearsal_checkpoint",
        lambda: {"cursor": 17, "opaque": "preserve-through-rollback"},
    )
    await ctx.sleep("rehearsal_sleep", 2.0)
    return {"resumed": True, "checkpoint": checkpoint, "input": params}
"#,
        ),
    ];
    for (name, source) in files {
        std::fs::write(workflow_dir.join(name), source)
            .unwrap_or_else(|error| panic!("write {name}: {error}"));
    }
}

async fn wait_for_forward_ready(client: &Client, base_url: &str, server: &mut ForwardProcess) {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Some(status) = server.exited() {
            panic!("reviewed forward binary exited before readiness: {status}");
        }
        match client.get(format!("{base_url}/readyz")).send().await {
            Ok(response) if response.status() == StatusCode::OK => return,
            Ok(response) => last = format!("readyz returned {}", response.status()),
            Err(error) => last = error.to_string(),
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("reviewed forward binary did not become ready: {last}");
}

async fn create_forward_workflow_run(
    client: &Client,
    base_url: &str,
    workflow_name: &str,
    idempotency_key: &str,
    input: Value,
) -> RehearsalTask {
    let response = client
        .post(format!("{base_url}/api/workflows/runs"))
        .header("Authorization", format!("Bearer {CONTROL_KEY}"))
        .json(&json!({
            "workflow_name": workflow_name,
            "input": input,
            "idempotency_key": idempotency_key,
            "max_attempts": 7,
        }))
        .send()
        .await
        .unwrap_or_else(|error| panic!("create forward workflow {workflow_name}: {error}"));
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .expect("decode forward workflow create response");
    assert_eq!(status, StatusCode::OK, "create {workflow_name}: {body}");
    let task_id = Uuid::parse_str(body["task_id"].as_str().expect("workflow create task_id"))
        .expect("workflow task UUID");
    let run_id = Uuid::parse_str(body["run_id"].as_str().expect("workflow create run_id"))
        .expect("workflow run UUID");
    RehearsalTask {
        queue: if workflow_name == "slack_sync" {
            "centaur_workflows_slack_live"
        } else {
            "centaur_workflows"
        },
        task_id,
        run_id,
    }
}

fn task_table(queue: &str) -> &'static str {
    match queue {
        "centaur_workflows" => "absurd.t_centaur_workflows",
        "centaur_workflows_slack_live" => "absurd.t_centaur_workflows_slack_live",
        "centaur_workflows_etl" => "absurd.t_centaur_workflows_etl",
        other => panic!("unsupported rehearsal queue {other}"),
    }
}

fn run_table(queue: &str) -> &'static str {
    match queue {
        "centaur_workflows" => "absurd.r_centaur_workflows",
        "centaur_workflows_slack_live" => "absurd.r_centaur_workflows_slack_live",
        "centaur_workflows_etl" => "absurd.r_centaur_workflows_etl",
        other => panic!("unsupported rehearsal queue {other}"),
    }
}

async fn wait_for_task_state(pool: &PgPool, queue: &str, task_id: Uuid, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(45);
    let table = task_table(queue);
    let mut last = String::new();
    while Instant::now() < deadline {
        match sqlx::query_scalar::<_, String>(&format!(
            "select state from {table} where task_id = $1::uuid"
        ))
        .bind(task_id.to_string())
        .fetch_optional(pool)
        .await
        {
            Ok(Some(state)) if state == expected => return,
            Ok(Some(state)) => last = state,
            Ok(None) => last = "missing".to_owned(),
            Err(error) => last = error.to_string(),
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("task {task_id} did not reach {expected}; last state {last}");
}

async fn expire_running_claim(pool: &PgPool, task: &RehearsalTask) {
    sqlx::query(&format!(
        "update {} set claim_expires_at = now() - interval '1 second' where run_id = $1::uuid",
        run_table(task.queue)
    ))
    .bind(task.run_id.to_string())
    .execute(pool)
    .await
    .expect("expire interrupted forward claim");
}

async fn convert_pending_to_event_wait(pool: &PgPool, task: &RehearsalTask) {
    let task_table = task_table(task.queue);
    let run_table = run_table(task.queue);
    sqlx::query(&format!(
        "update {task_table} set state = 'sleeping' where task_id = $1::uuid"
    ))
    .bind(task.task_id.to_string())
    .execute(pool)
    .await
    .expect("set waiting task state");
    sqlx::query(&format!(
        "update {run_table} set state = 'sleeping', claimed_by = null, \
         claim_expires_at = null, available_at = '2099-01-01T00:00:00Z', \
         wake_event = 'rollback.rehearsal.resume' where run_id = $1::uuid"
    ))
    .bind(task.run_id.to_string())
    .execute(pool)
    .await
    .expect("set waiting run state");
    sqlx::query(&format!(
        "insert into absurd.c_{} \
         (task_id, checkpoint_name, state, status, owner_run_id, updated_at) \
         values ($1::uuid, 'event-checkpoint', $2, 'committed', $3::uuid, now())",
        task.queue
    ))
    .bind(task.task_id.to_string())
    .bind(json!({"cursor": "event-wait-preserved"}))
    .bind(task.run_id.to_string())
    .execute(pool)
    .await
    .expect("seed waiting checkpoint");
    sqlx::query(&format!(
        "insert into absurd.w_{} \
         (task_id, run_id, step_name, event_name, timeout_at, created_at) \
         values ($1::uuid, $2::uuid, 'await-rehearsal', 'rollback.rehearsal.resume', \
                 '2099-01-01T00:00:00Z', now())",
        task.queue
    ))
    .bind(task.task_id.to_string())
    .bind(task.run_id.to_string())
    .execute(pool)
    .await
    .expect("seed waiting row");
}

async fn customize_retry_and_cancellation_contract<'a>(
    pool: &PgPool,
    tasks: impl IntoIterator<Item = &'a RehearsalTask>,
) {
    for task in tasks {
        sqlx::query(&format!(
            "update {} set retry_strategy = $2, cancellation = $3 where task_id = $1::uuid",
            task_table(task.queue)
        ))
        .bind(task.task_id.to_string())
        .bind(json!({
            "kind": "exponential",
            "base_seconds": 0.125,
            "factor": 2.0,
            "max_seconds": 9.0,
        }))
        .bind(json!({"max_duration": 3600, "max_delay": 60}))
        .execute(pool)
        .await
        .expect("set rehearsal task policy");
    }
}

async fn task_contract_snapshot<'a>(
    pool: &PgPool,
    tasks: impl IntoIterator<Item = &'a RehearsalTask>,
) -> Value {
    let mut snapshot = Map::new();
    for task in tasks {
        let table = task_table(task.queue);
        let value = sqlx::query_scalar::<_, Value>(&format!(
            "select jsonb_build_object( \
                'params', params, 'headers', headers, 'retry_strategy', retry_strategy, \
                'max_attempts', max_attempts, 'cancellation', cancellation, \
                'idempotency_key', idempotency_key) \
             from {table} where task_id = $1::uuid"
        ))
        .bind(task.task_id.to_string())
        .fetch_one(pool)
        .await
        .expect("snapshot task contract");
        snapshot.insert(format!("{}:{}", task.queue, task.task_id), value);
    }
    Value::Object(snapshot)
}

async fn canonical_cross_version_snapshot(pool: &PgPool) -> Value {
    let mut snapshot = Map::new();
    for table in [
        "public._sqlx_migrations",
        "public.sessions",
        "absurd.t_centaur_workflows",
        "absurd.r_centaur_workflows",
        "absurd.c_centaur_workflows",
        "absurd.e_centaur_workflows",
        "absurd.w_centaur_workflows",
        "absurd.t_centaur_workflows_slack_live",
        "absurd.r_centaur_workflows_slack_live",
        "absurd.c_centaur_workflows_slack_live",
        "absurd.e_centaur_workflows_slack_live",
        "absurd.w_centaur_workflows_slack_live",
        "absurd.t_centaur_workflows_etl",
        "absurd.r_centaur_workflows_etl",
        "absurd.c_centaur_workflows_etl",
        "absurd.e_centaur_workflows_etl",
        "absurd.w_centaur_workflows_etl",
    ] {
        snapshot.insert(table.to_owned(), canonical_rows(pool, table).await);
    }
    Value::Object(snapshot)
}

async fn rollback_schema_identity_snapshot(pool: &PgPool) -> Value {
    let queues = sqlx::query_scalar::<_, String>(
        "select queue_name from absurd.list_queues() order by queue_name",
    )
    .fetch_all(pool)
    .await
    .expect("snapshot absurd queue registry");
    let absurd_objects = sqlx::query_scalar::<_, Value>(
        r#"
        select coalesce(
            jsonb_agg(
                jsonb_build_object('name', c.relname, 'kind', c.relkind::text)
                order by c.relname, c.relkind::text
            ),
            '[]'::jsonb
        )
        from pg_class c
        join pg_namespace n on n.oid = c.relnamespace
        where n.nspname = 'absurd'
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("snapshot absurd schema objects");
    json!({
        "migration_ledger": canonical_rows(pool, "public._sqlx_migrations").await,
        "queues": queues,
        "absurd_objects": absurd_objects,
    })
}

async fn emit_forward_workflow_event(client: &Client, base_url: &str, event_name: &str) {
    let response = client
        .post(format!("{base_url}/api/workflows/events"))
        .header("Authorization", format!("Bearer {CONTROL_KEY}"))
        .json(&json!({"event_name": event_name, "payload": {"resumed": true}}))
        .send()
        .await
        .expect("emit re-forward workflow event");
    assert_eq!(response.status(), StatusCode::OK);
}

async fn execute_reassigned_session(
    client: &Client,
    base_url: &str,
    reassigned: &ReassignedSession,
) {
    let input_line = serde_json::to_string(&json!({
        "type": "user",
        "model": "rollback-rehearsal-model",
        "message": {"role": "user", "content": [{"type": "text", "text": "PONG"}]},
    }))
    .expect("serialize rehearsal input");
    let url = format!(
        "{base_url}/api/session/{}/execute",
        urlencoding::encode(reassigned.thread_key.as_str())
    );
    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {CONTROL_KEY}"))
        .json(&json!({
            "idempotency_key": format!("re-forward-{}", Uuid::new_v4()),
            "metadata": {"source": "rollback-cross-version-rehearsal"},
            "input_lines": [input_line],
            "idle_timeout_ms": 5_000,
            "max_duration_ms": 15_000,
        }))
        .send()
        .await
        .expect("execute rollback-era session after re-forward");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "re-forward execute failed: {body}");
}

async fn wait_for_forward_sandbox_replacement(pool: &PgPool, reassigned: &ReassignedSession) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = None;
    while Instant::now() < deadline {
        let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "select sandbox_id, sandbox_content_revision from sessions where thread_key = $1",
        )
        .bind(reassigned.thread_key.as_str())
        .fetch_one(pool)
        .await
        .expect("read re-forward session assignment");
        if row.0.as_deref() != Some(reassigned.rollback_sandbox_id.as_str())
            && row.0.is_some()
            && row.1.as_deref() != Some(reassigned.stale_forward_stamp.as_str())
            && row.1.is_some()
        {
            return;
        }
        last = Some(row);
        sleep(Duration::from_millis(100)).await;
    }
    panic!("re-forward did not replace and restamp rollback-era sandbox: {last:?}");
}

async fn assert_checkpoint_survived_resume(pool: &PgPool, task: &RehearsalTask) {
    let rows = sqlx::query_scalar::<_, Value>(&format!(
        "select coalesce(jsonb_agg(state order by checkpoint_name), '[]'::jsonb) \
         from absurd.c_{} where task_id = $1::uuid",
        task.queue
    ))
    .bind(task.task_id.to_string())
    .fetch_one(pool)
    .await
    .expect("read resumed checkpoints");
    assert!(
        rows.as_array().is_some_and(|rows| {
            rows.iter()
                .any(|state| state["opaque"] == "preserve-through-rollback")
        }),
        "sleeping workflow checkpoint did not survive re-forward: {rows}"
    );
}

fn assert_fixture_provenance() {
    let forward_commit = FORWARD_COMMIT_FILE.trim();
    assert!(
        forward_commit.len() == 40
            && forward_commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        ".github/rollback-bridge-reviewed-forward-commit must contain the frozen lowercase 40-character commit SHA"
    );
    let expected = include_str!("fixtures/forward_migrations/SHA256SUMS")
        .lines()
        .filter_map(|line| line.split_once("  "))
        .map(|(checksum, file)| (file.to_owned(), checksum.to_owned()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(expected.len(), FORWARD_MIGRATIONS.len());
    for ((file, sql), (embedded_file, embedded_sql)) in FORWARD_MIGRATIONS
        .into_iter()
        .zip(EMBEDDED_FORWARD_MIGRATIONS)
    {
        assert_eq!(file, embedded_file);
        assert_eq!(
            sql, embedded_sql,
            "embedded production migration {file} differs from its reviewed fixture"
        );
        let checksum = hex::encode(Sha256::digest(sql.as_bytes()));
        assert_eq!(
            expected.get(file).map(String::as_str),
            Some(checksum.as_str()),
            "test-only forward migration {file} drifted from pinned commit {forward_commit}"
        );
    }
}

struct SeededRows {
    running_run_id: String,
}

struct ReassignedSession {
    thread_key: ThreadKey,
    forward_content_generation: String,
    rollback_sandbox_id: String,
    stale_forward_stamp: String,
}

async fn seed_forward_assignment_and_simulate_bridge_reassignment(
    store: &PgSessionStore,
    pool: &PgPool,
) -> ReassignedSession {
    let thread_key = ThreadKey::parse(format!(
        "rollback:content-revision:{}",
        Uuid::new_v4().simple()
    ))
    .expect("forward assignment thread key");
    store
        .create_or_get_session(&thread_key, &HarnessType::Codex, None, json!({}))
        .await
        .expect("create forward assignment session");

    let forward_content_generation = "sandbox-spec-sha256:forward-reviewed-generation".to_owned();
    let forward_sandbox_id = "asbx-forward-content";
    let rollback_sandbox_id = "asbx-rollback-content".to_owned();
    let stale_forward_stamp =
        forward_assignment_content_revision(&forward_content_generation, forward_sandbox_id);
    sqlx::query(
        "update sessions \
         set sandbox_id = $2, sandbox_content_revision = $3, \
             sandbox_repo_cache_enabled = true, sandbox_observability_enabled = true \
         where thread_key = $1",
    )
    .bind(thread_key.as_str())
    .bind(forward_sandbox_id)
    .bind(&stale_forward_stamp)
    .execute(pool)
    .await
    .expect("seed forward content-stamped assignment");

    let execution_id = store
        .create_execution(&thread_key, None, json!({"rollback_bridge": true}))
        .await
        .expect("create rollback bridge assignment execution")
        .execution
        .execution_id;
    let assigned = store
        .assign_sandbox_to_active_execution(
            &thread_key,
            &execution_id,
            Some(forward_sandbox_id),
            &rollback_sandbox_id,
            &SandboxCapabilities::default_enabled(),
        )
        .await
        .expect("bridge assignment against forward schema")
        .expect("active bridge assignment wins");
    assert_eq!(
        assigned.sandbox_id.as_deref(),
        Some(rollback_sandbox_id.as_str())
    );
    store
        .complete_execution(&execution_id)
        .await
        .expect("complete bridge assignment fixture execution");

    ReassignedSession {
        thread_key,
        forward_content_generation,
        rollback_sandbox_id,
        stale_forward_stamp,
    }
}

async fn assert_reassignment_requires_forward_replacement(
    pool: &PgPool,
    reassigned: &ReassignedSession,
) {
    let (sandbox_id, persisted_stamp) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "select sandbox_id, sandbox_content_revision from sessions where thread_key = $1",
    )
    .bind(reassigned.thread_key.as_str())
    .fetch_one(pool)
    .await
    .expect("read rollback-era assignment after bridge startup");
    assert_eq!(
        sandbox_id.as_deref(),
        Some(reassigned.rollback_sandbox_id.as_str())
    );
    assert_eq!(
        persisted_stamp.as_deref(),
        Some(reassigned.stale_forward_stamp.as_str())
    );

    let desired_re_forward_stamp = forward_assignment_content_revision(
        &reassigned.forward_content_generation,
        &reassigned.rollback_sandbox_id,
    );
    assert_ne!(
        reassigned.stale_forward_stamp, desired_re_forward_stamp,
        "a rollback-era sandbox must not authenticate as current forward boot content"
    );
}

fn forward_assignment_content_revision(generation: &str, sandbox_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"centaur-sandbox-assignment-v1\0");
    digest.update(generation.as_bytes());
    digest.update(b"\0");
    digest.update(sandbox_id.as_bytes());
    format!("sandbox-assignment-sha256:{:x}", digest.finalize())
}

async fn seed_representative_workflow_rows(pool: &PgPool) -> SeededRows {
    let states = ["pending", "running", "sleeping", "pending", "pending"];
    let mut running_run_id = None;
    for (index, (queue, state)) in QUEUES.iter().zip(states).enumerate() {
        let task_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        if state == "running" {
            running_run_id = Some(run_id);
        }
        let task_name = if *queue == "centaur_workflow_schedules" {
            "centaur.workflow.schedule_tick"
        } else {
            "centaur.workflow"
        };
        let workflow_name = if index == 0 {
            // Present only in the forward overlay: an active rollback worker
            // would fail or reap this row.
            "fineas_google_drive_folder_sync"
        } else {
            [
                "slack_live_forward",
                "sleeping_forward",
                "etl_backfill_forward",
                "schedule_forward",
            ][index - 1]
        };
        let task_table = format!("absurd.t_{queue}");
        let run_table = format!("absurd.r_{queue}");
        sqlx::query(&format!(
            "insert into {task_table} \
             (task_id, task_name, params, headers, retry_strategy, max_attempts, cancellation, \
              enqueue_at, first_started_at, state, attempts, last_attempt_run, idempotency_key) \
             values ($1::uuid, $2, $3, $4, $5, 7, $6, \
                     '2026-07-01T01:02:03Z', '2026-07-01T01:03:03Z', $7, 2, $8::uuid, $9)"
        ))
        .bind(task_id.to_string())
        .bind(task_name)
        .bind(json!({
            "workflow_name": workflow_name,
            "input": {"fixture": index, "forward_only": index == 0},
            "harness_type": "codex"
        }))
        .bind(json!({"traceparent": format!("fixture-{index}"), "x-forward": true}))
        .bind(json!({"type": "exponential", "initial_ms": 125, "max_ms": 9000}))
        .bind(json!({"mode": "cooperative", "grace_ms": 4321}))
        .bind(state)
        .bind(run_id.to_string())
        .bind(format!("rollback-preservation-{index}"))
        .execute(pool)
        .await
        .expect("seed forward task row");

        let (claimed_by, claim_expires_at, wake_event, available_at) = match state {
            "running" => (
                Some("forward-worker"),
                Some("2026-07-01T01:04:03Z"),
                None,
                "2026-07-01T01:02:03Z",
            ),
            "sleeping" => (None, None, Some("forward.resume"), "2099-07-01T01:02:03Z"),
            _ => (None, None, None, "2026-07-01T01:02:03Z"),
        };
        sqlx::query(&format!(
            "insert into {run_table} \
             (run_id, task_id, attempt, state, claimed_by, claim_expires_at, available_at, \
              wake_event, event_payload, started_at, result, failure_reason, created_at) \
             values ($1::uuid, $2::uuid, 2, $3, $4, $5::timestamptz, $6::timestamptz, \
                     $7, $8, '2026-07-01T01:03:03Z', $9, $10, '2026-07-01T01:02:03Z')"
        ))
        .bind(run_id.to_string())
        .bind(task_id.to_string())
        .bind(state)
        .bind(claimed_by)
        .bind(claim_expires_at)
        .bind(available_at)
        .bind(wake_event)
        .bind(json!({"delivery": "preserve"}))
        .bind(json!({"partial": true}))
        .bind(json!({"previous_attempt": "preserve"}))
        .execute(pool)
        .await
        .expect("seed forward run row");

        if state == "sleeping" {
            sqlx::query(&format!(
                "insert into absurd.c_{queue} \
                 (task_id, checkpoint_name, state, status, owner_run_id, updated_at) \
                 values ($1::uuid, 'forward-checkpoint', $2, 'committed', $3::uuid, \
                         '2026-07-01T01:05:03Z')"
            ))
            .bind(task_id.to_string())
            .bind(json!({"cursor": "opaque-forward-cursor", "page": 17}))
            .bind(run_id.to_string())
            .execute(pool)
            .await
            .expect("seed sleeping checkpoint");
            sqlx::query(&format!(
                "insert into absurd.e_{queue} (event_name, payload, emitted_at) \
                 values ('forward.resume', $1, '2026-07-01T01:05:03Z')"
            ))
            .bind(json!({"event": "must-remain"}))
            .execute(pool)
            .await
            .expect("seed sleeping event");
            sqlx::query(&format!(
                "insert into absurd.w_{queue} \
                 (task_id, run_id, step_name, event_name, timeout_at, created_at) \
                 values ($1::uuid, $2::uuid, 'await-forward', 'forward.resume', \
                         '2099-07-01T01:02:03Z', '2026-07-01T01:05:03Z')"
            ))
            .bind(task_id.to_string())
            .bind(run_id.to_string())
            .execute(pool)
            .await
            .expect("seed sleeping wait");
        }
    }
    SeededRows {
        running_run_id: running_run_id.expect("running fixture run").to_string(),
    }
}

async fn assert_seeded_states(pool: &PgPool) {
    let mut counts = BTreeMap::new();
    for queue in QUEUES {
        let rows = sqlx::query_as::<_, (String, i64)>(&format!(
            "select state, count(*) from absurd.t_{queue} group by state order by state"
        ))
        .fetch_all(pool)
        .await
        .expect("count seeded states");
        counts.insert(queue, rows);
    }
    assert_eq!(counts["centaur_workflows"], vec![("pending".to_owned(), 1)]);
    assert_eq!(
        counts["centaur_workflows_slack_live"],
        vec![("running".to_owned(), 1)]
    );
    assert_eq!(
        counts["centaur_workflows_etl"],
        vec![("sleeping".to_owned(), 1)]
    );
    assert_eq!(
        counts["centaur_workflows_etl_backfill"],
        vec![("pending".to_owned(), 1)]
    );
    assert_eq!(
        counts["centaur_workflow_schedules"],
        vec![("pending".to_owned(), 1)]
    );
}

async fn canonical_forward_snapshot(pool: &PgPool) -> Value {
    let mut snapshot = Map::new();
    snapshot.insert(
        "_sqlx_migrations".to_owned(),
        canonical_rows(pool, "public._sqlx_migrations").await,
    );
    snapshot.insert(
        "absurd.queues".to_owned(),
        canonical_rows(pool, "absurd.queues").await,
    );
    snapshot.insert(
        "public.sessions".to_owned(),
        canonical_rows(pool, "public.sessions").await,
    );
    for queue in QUEUES {
        for prefix in ["t", "r", "c", "e", "w"] {
            let table = format!("absurd.{prefix}_{queue}");
            snapshot.insert(table.clone(), canonical_rows(pool, &table).await);
        }
    }
    Value::Object(snapshot)
}

async fn canonical_rows(pool: &PgPool, table: &str) -> Value {
    sqlx::query_scalar::<_, Value>(&format!(
        "select coalesce( \
             jsonb_agg(to_jsonb(snapshot_row) order by to_jsonb(snapshot_row)::text), \
             '[]'::jsonb) \
         from (select * from {table}) snapshot_row"
    ))
    .fetch_one(pool)
    .await
    .unwrap_or_else(|error| panic!("snapshot {table}: {error}"))
}

async fn exercise_read_and_mutation_lanes(client: &Client, base_url: &str, running_run_id: &str) {
    let control = format!("Bearer {CONTROL_KEY}");
    for path in ["/api/workflows/runs?limit=50", "/api/workflows/schedules"] {
        let response = client
            .get(format!("{base_url}{path}"))
            .header("Authorization", &control)
            .send()
            .await
            .expect("read paused workflow state");
        assert_eq!(response.status(), StatusCode::OK, "GET {path}");
    }

    let mutations = [
        (
            "/api/workflows/runs".to_owned(),
            json!({"workflow_name": "must_not_start", "input": {"fixture": true}}),
        ),
        (
            format!("/api/workflows/runs/{running_run_id}/cancel"),
            json!({}),
        ),
        (
            "/api/workflows/events".to_owned(),
            json!({"event_name": "forward.resume", "payload": {"must_not_write": true}}),
        ),
    ];
    for (path, body) in mutations {
        let response = client
            .post(format!("{base_url}{path}"))
            .header("Authorization", &control)
            .json(&body)
            .send()
            .await
            .expect("exercise paused workflow mutation");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "POST {path}");
    }

    let webhook = client
        .post(format!(
            "{base_url}/api/webhooks/trivy-vulnerability-intake"
        ))
        .header("Authorization", format!("Bearer {WEBHOOK_KEY}"))
        .json(&json!({"alerts": [{"fixture": true}]}))
        .send()
        .await
        .expect("exercise authenticated webhook mutation lane");
    assert_eq!(webhook.status(), StatusCode::FORBIDDEN);

    let admin = client
        .post(format!("{base_url}/api/admin/slack/dm-sync/batch"))
        .header("Authorization", &control)
        .json(&json!({}))
        .send()
        .await
        .expect("exercise control-authenticated admin lane");
    assert_eq!(admin.status(), StatusCode::OK);
}

async fn wait_for_ready(client: &Client, base_url: &str, server: &mut BridgeProcess) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Some(status) = server.exited() {
            panic!("rollback bridge exited before readiness: {status}");
        }
        match client.get(format!("{base_url}/readyz")).send().await {
            Ok(response) if response.status() == StatusCode::OK => return,
            Ok(response) => last = format!("readyz returned {}", response.status()),
            Err(error) => last = error.to_string(),
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("rollback bridge did not become ready: {last}");
}

fn unused_local_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral test port");
    listener
        .local_addr()
        .expect("ephemeral local address")
        .port()
}

fn database_url_with_name(admin_url: &str, database_name: &str) -> String {
    let (without_query, query) = admin_url
        .split_once('?')
        .map_or((admin_url, ""), |(base, query)| (base, query));
    let slash = without_query
        .rfind('/')
        .expect("database URL must include a path");
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    format!("{}{database_name}{query}", &without_query[..=slash])
}
