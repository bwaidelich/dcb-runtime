//! A host for the DCB Runtime component over UmaDB.
//!
//!     dcb-runtime-host serve <model.json>                     an HTTP API for the model's commands
//!     dcb-runtime-host run <model.json> <command> <args-json>  decide one command, print the outcome
//!
//! See `--help` for the options; each also reads an environment variable.

mod component;
mod http;

use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use umadb_client::AsyncUmaDbClient;
use wasmtime::{Result, format_err};

use component::Pool;

#[derive(Parser)]
#[command(version, about = "Runs a DCB Model against UmaDB")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Common {
    /// The model, as the playground exports it (JSON).
    model: String,
    /// The UmaDB server.
    #[arg(long, env = "UMADB_URL", default_value = "http://localhost:50051")]
    store: String,
    /// The runtime component.
    #[arg(long, env = "DCB_RUNTIME_WASM", default_value = "build/dcb-runtime.wasm")]
    wasm: String,
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
        /// Do not serve Swagger UI at /docs.
        #[arg(long, env = "DCB_NO_DOCS")]
        no_docs: bool,
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
    let client = AsyncUmaDbClient::connect(common.store.clone(), None, None, None)
        .await
        .map_err(|e| format_err!("connecting to {}: {e}", common.store))?;
    Pool::new(&common.wasm, model, client, size).await
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve { common, listen, instances, no_docs } => {
            let size = instances.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
            let api = Arc::new(http::Api::new(pool(&common, size).await?).await?);
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            eprintln!("serving {} on http://{listen}{}", common.model, if no_docs { "" } else { " (docs at /docs)" });
            axum::serve(listener, http::router(api, !no_docs))
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
