//! A host for the DCB Runtime component over a DCB event store – UmaDB,
//! Tephra, PostgreSQL, Axon Server or EventSourcingDB (`store.rs`).
//!
//!     dcb-runtime-host serve <model.dcb.json>                      an HTTP API for the model's commands
//!     dcb-runtime-host run <model.dcb.json> <command> <args-json>  decide one command, print the outcome
//!
//! See `--help` for the options; each also reads an environment variable.

mod component;
mod http;
mod store;

use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use wasmtime::{Result, format_err};

use component::Pool;
use store::EventStore;

#[derive(Parser)]
#[command(version, about = "Runs a DCB Model against a DCB event store")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Common {
    /// The model, as the playground exports it (JSON).
    model: String,
    /// The event store: umadb://host:port (or umadb+tls://) for UmaDB, tephra://host:port for Tephra,
    /// postgres://user:pw@host:port/db for PostgreSQL, axonserver://host:port[/context] (or
    /// axonserver+tls://) for Axon Server, eventsourcingdb://token@host:port (or eventsourcingdb+tls://)
    /// for EventSourcingDB.
    #[arg(long, env = "DCB_STORE", default_value = "umadb://localhost:50051")]
    store: String,
    /// How long a call to the store may take before it counts as failed.
    #[arg(long, env = "DCB_STORE_TIMEOUT", default_value = "10s", value_parser = parse_duration)]
    store_timeout: std::time::Duration,
    /// A runtime component to use instead of the one built into the host.
    #[arg(long, env = "DCB_RUNTIME_WASM")]
    wasm: Option<String>,
}

/// `500ms`, `10s` or `2m`.
fn parse_duration(text: &str) -> std::result::Result<std::time::Duration, String> {
    let split = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let n: u64 = number.parse().map_err(|_| format!("not a duration: {text}"))?;
    match unit {
        "ms" => Ok(std::time::Duration::from_millis(n)),
        "s" | "" => Ok(std::time::Duration::from_secs(n)),
        "m" => Ok(std::time::Duration::from_secs(n * 60)),
        _ => Err(format!("not a duration: {text} (use ms, s or m)")),
    }
}

#[derive(Subcommand)]
enum Command {
    /// Serve the model's commands over HTTP.
    Serve {
        #[command(flatten)]
        common: Common,
        /// The address to listen on.
        #[arg(long, env = "DCB_LISTEN", default_value = "127.0.0.1:8080")]
        listen: String,
        /// How many commands are decided at once; each holds an instance
        /// of the component. Defaults to the number of CPUs.
        #[arg(long, env = "DCB_INSTANCES")]
        instances: Option<usize>,
        /// Serve Swagger UI at /docs.
        #[arg(long, env = "DCB_WITH_DOCS")]
        with_docs: bool,
        /// Serve POST /read: the events matching a DCB query, a page at a time.
        #[arg(long, env = "DCB_WITH_READ")]
        with_read: bool,
    },
    /// Decide one command and print the outcome as JSON.
    Run {
        #[command(flatten)]
        common: Common,
        /// The command's name.
        command: String,
        /// Its arguments, as a JSON object.
        args: String,
    },
}

async fn pool(common: &Common, size: usize) -> Result<Pool> {
    let model = std::fs::read_to_string(&common.model).map_err(|e| format_err!("reading {}: {e}", common.model))?;
    let store = EventStore::connect(&common.store, common.store_timeout).await?;
    Pool::new(common.wasm.as_deref(), model, store, size).await
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve { common, listen, instances, with_docs, with_read } => {
            let size = instances.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
            let api = Arc::new(http::Api::new(pool(&common, size).await?, with_read).await?);
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            eprintln!("serving {} on http://{listen}{}", common.model, if with_docs { " (docs at /docs)" } else { "" });
            axum::serve(listener, http::router(api, with_docs))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Command::Run { common, command, args } => {
            let outcome = pool(&common, 1).await?.handle(&command, &args).await?;
            let response = http::outcome_response(outcome);
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
            println!("{}", String::from_utf8_lossy(&body));
            if !status.is_success() {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
