//! `centaur-codemode`: run agent-written JavaScript cells against TypeScript
//! connectors.
//!
//! ```text
//! centaur-codemode exec  [-e CODE | -]   run one cell and print the JSON response
//! centaur-codemode list                  one line per connector: name and description
//! centaur-codemode search QUERY          rank connector functions for a task
//! centaur-codemode load CONNECTOR        print one connector's declarations
//! centaur-codemode brief                 print the typed catalog an agent reads
//! centaur-codemode serve                 HTTP API for api-rs
//! ```
//!
//! Every command honors the deployment connector policy. The CLI tool filter
//! and HTTP request policies can narrow it further.

mod catalog;
mod engine;
mod files;
mod reload;
mod search;
mod server;
mod telemetry;
mod websocket;

use std::{io::Read, net::SocketAddr, path::PathBuf, sync::Arc};

use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr, bail};

use crate::{
    catalog::{Catalog, Policy},
    engine::{Engine, ExecRequest},
    reload::Reloader,
};

#[derive(Parser)]
#[command(version, about = "Run Code Mode cells against TypeScript connectors")]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Common {
    /// Connector roots, in overlay order (later roots shadow earlier ones).
    /// Missing roots (overlays a principal cannot see) are skipped.
    #[arg(
        long = "connectors",
        env = "CENTAUR_CODEMODE_CONNECTORS",
        value_delimiter = ':',
        default_value = "connectors",
        global = true
    )]
    roots: Vec<PathBuf>,
    /// Extra PEM roots to trust, such as the iron-proxy CA.
    #[arg(long, env = "CENTAUR_CODEMODE_CA_FILE", global = true)]
    ca_file: Option<PathBuf>,
    /// Sandbox context file, given to connectors as `ctx.context`. Defaults
    /// to `~/.centaur/sandbox.json`, where harness-server writes it; in a Code
    /// Mode sandbox it is absent and `serve` asks Console instead.
    #[arg(long, env = "CENTAUR_SANDBOX_CONTEXT_FILE", global = true)]
    context_file: Option<PathBuf>,
    /// Turns on the files capability: connectors may read and write files
    /// under this directory (in a Centaur sandbox, the agent's workspace).
    #[arg(long, env = "CENTAUR_CODEMODE_FILES_DIR", global = true)]
    files_dir: Option<PathBuf>,
    /// Connectors the CLI exposes (comma-separated), within the deployment's
    /// tool filter; all when unset. `serve` takes a per-request policy
    /// instead.
    #[arg(long, value_delimiter = ',', global = true)]
    tools: Option<Vec<String>>,
}

impl Common {
    /// Additional connector filter for CLI commands.
    fn policy(&self) -> Option<Policy> {
        self.tools.as_ref().map(|tools| {
            Policy::from_allowlist(
                tools
                    .iter()
                    .map(|tool| tool.trim())
                    .filter(|tool| !tool.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )
        })
    }
}

#[derive(Subcommand)]
enum Command {
    /// Run one cell and print the JSON response.
    Exec {
        /// Cell source; `-` (or nothing) reads standard input.
        #[arg(short = 'e', long = "code")]
        code: Option<String>,
        /// Print only the result (or the error) instead of the full response.
        #[arg(long)]
        result_only: bool,
    },
    /// Print one line per visible connector: its name and description.
    List,
    /// Rank connector functions for a task and print the JSON results.
    Search {
        /// Service names, URLs, domains and the requested action.
        query: String,
        /// Maximum number of results (default 8, at most 50).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Print one connector's TypeScript declarations and JSDoc.
    Load {
        /// Connector name, as `tools.<connector>` in a cell.
        connector: String,
    },
    /// Print the typed catalog an agent reads before writing a cell.
    Brief,
    /// Serve the HTTP API.
    Serve {
        /// Address to listen on; sandboxes set `0.0.0.0:8790`.
        #[arg(
            long,
            env = "CENTAUR_CODEMODE_LISTEN",
            default_value = "127.0.0.1:8790"
        )]
        listen: SocketAddr,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // `serve` logs requests for operators. The other commands run as agent
    // tool calls, whose stderr lands in the agent's context: only problems.
    let default_filter = match cli.command {
        Command::Serve { .. } => "centaur_codemode=info,warn",
        // A failed span export is not the agent's problem.
        _ => "warn,opentelemetry=off,opentelemetry_sdk=off,opentelemetry_otlp=off",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        // Pod logs are not a terminal; color codes would end up in them.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let Cli { common, command } = cli;
    // Only `serve` hides `@requires files` functions when it has no files
    // capability: it backs MCP clients that cannot use them. The CLI shows
    // the whole catalog, and such a call fails with a clear error instead.
    let files = !matches!(command, Command::Serve { .. }) || common.files_dir.is_some();
    let mut catalog = Catalog::load(&common.roots, files)?;
    catalog.deployment_policy = Policy::from_env(|name| {
        std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
    });
    let catalog = Arc::new(catalog);
    match command {
        Command::List => {
            let policy = common.policy();
            for connector in catalog.visible(policy.as_ref()) {
                println!("{}: {}", connector.name, connector.description);
            }
            Ok(())
        }
        Command::Search { query, limit } => {
            let policy = common.policy();
            let results = catalog.search(&query, search::limit(limit), policy.as_ref());
            println!("{}", serde_json::to_string_pretty(&results)?);
            Ok(())
        }
        Command::Load { connector } => {
            let policy = common.policy();
            let Some(found) = catalog.find(&connector, policy.as_ref()) else {
                eprintln!("unknown connector {connector}; run `centaur-codemode list`");
                std::process::exit(1);
            };
            print!("{}", catalog::connector_brief(found));
            Ok(())
        }
        Command::Brief => {
            print!("{}", catalog.brief(common.policy().as_ref()));
            Ok(())
        }
        Command::Exec { code, result_only } => {
            let code = match code.as_deref() {
                Some("-") | None => {
                    let mut code = String::new();
                    std::io::stdin().read_to_string(&mut code)?;
                    code
                }
                Some(code) => code.to_owned(),
            };
            let engine = engine(&common, catalog)?;
            let response = engine
                .exec(ExecRequest {
                    code,
                    tools: None,
                    policy: common.policy(),
                    trace: telemetry::TraceParent::for_cli(),
                })
                .await;
            // Before any exit: the process ends right after this.
            telemetry::flush();
            let ok = response.ok;
            if result_only {
                match (&response.result, &response.error) {
                    (Some(result), _) => println!("{}", serde_json::to_string_pretty(result)?),
                    (None, Some(error)) => eprintln!("{}", error.message),
                    (None, None) => {}
                }
            } else {
                println!("{}", serde_json::to_string_pretty(&response)?);
            }
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Serve { listen } => {
            if common.tools.is_some() {
                tracing::warn!("--tools applies to exec and brief; serve uses the request policy");
            }
            let mut engine = engine(&common, catalog)?;
            engine.enable_console_context();
            engine.prefetch_context();
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .wrap_err_with(|| format!("binding {listen}"))?;
            let reloader = tokio::spawn(
                Reloader::new(common.roots.clone(), server::RELOAD_INTERVAL).run(engine.clone()),
            );
            tracing::info!(%listen, "centaur-codemode serving");
            let served = server::serve(listener, engine, server::Config::default()).await;
            reloader.abort();
            telemetry::flush();
            served?;
            Ok(())
        }
    }
}

fn engine(common: &Common, catalog: Arc<Catalog>) -> Result<Engine> {
    if catalog.connectors.is_empty() {
        bail!(
            "no connectors found under {}",
            common
                .roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(":")
        );
    }
    let context_file = common.context_file.clone().or_else(|| {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".centaur/sandbox.json"))
    });
    Engine::new(
        catalog,
        common.ca_file.as_deref(),
        context_file,
        common.files_dir.as_deref(),
    )
}
