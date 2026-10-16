//! A host for the DCB Runtime component over a DCB event store – UmaDB,
//! Tephra, PostgreSQL, Axon Server, EventSourcingDB or one in memory
//! (`store.rs`).
//!
//!     dcb-runtime-host serve <model.dcb.json>                      an HTTP API for the model's commands
//!     dcb-runtime-host run <model.dcb.json> <command> <args-json>  decide one command, print the outcome
//!
//! The model is a file or an http(s):// URL, fetched once on start.
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
    /// The event store: umadb://host:port (or umadb+tls://) for UmaDB, tephra://host:port for Tephra,
    /// postgres://user:pw@host:port/db for PostgreSQL, axonserver://host:port[/context] (or
    /// axonserver+tls://) for Axon Server, eventsourcingdb://token@host:port (or eventsourcingdb+tls://)
    /// for EventSourcingDB, memory:// for one in this process, gone when it ends.
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
        /// The model, as the playground exports it (JSON): a file, or an http(s):// URL to fetch it from.
        #[arg(env = "DCB_MODEL")]
        model: String,
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
        /// The model, as the playground exports it (JSON): a file, or an http(s):// URL to fetch it from.
        model: String,
        #[command(flatten)]
        common: Common,
        /// The command's name.
        command: String,
        /// Its arguments, as a JSON object.
        args: String,
    },
}

/// The model's JSON, from a file or fetched from an http(s):// URL.
async fn model(source: &str) -> Result<String> {
    if !(source.starts_with("http://") || source.starts_with("https://")) {
        return std::fs::read_to_string(source).map_err(|e| format_err!("reading {source}: {e}"));
    }
    let fail = |e: String| format_err!("fetching {source}: {e}");
    let response = reqwest::get(source).await.map_err(|e| fail(e.to_string()))?;
    if !response.status().is_success() {
        return Err(fail(response.status().to_string()));
    }
    let text = response.text().await.map_err(|e| fail(e.to_string()))?;
    serde_json::from_str::<serde_json::Value>(&text).map_err(|e| fail(format!("no JSON: {e}")))?;
    Ok(text)
}

async fn pool(model_source: &str, common: &Common, size: usize) -> Result<Pool> {
    let model = model(model_source).await?;
    let store = EventStore::connect(&common.store, common.store_timeout).await?;
    Pool::new(common.wasm.as_deref(), model, store, size).await
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve { model, common, listen, instances, with_docs, with_read } => {
            let size = instances.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
            let api = Arc::new(http::Api::new(pool(&model, &common, size).await?, with_read).await?);
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            eprintln!("serving {model} on http://{listen}{}", if with_docs { " (docs at /docs)" } else { "" });
            if common.store.starts_with("memory:") {
                eprintln!("warning: the events are kept in memory – gone when the host stops");
            }
            if !listener.local_addr()?.ip().is_loopback() {
                eprintln!(
                    "warning: listening beyond this machine. This API is the core of your application, not its edge: \
                     call it from your application's adapters over a private network, never expose it directly. \
                     See https://github.com/bwaidelich/dcb-runtime#not-a-public-api"
                );
            }
            axum::serve(listener, http::router(api, with_docs))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Command::Run { model, common, command, args } => {
            let outcome = pool(&model, &common, 1).await?.handle(&command, &args).await?;
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
