//! A host for the DCB Runtime component over a DCB event store – UmaDB,
//! Tephra, PostgreSQL, Axon Server, EventSourcingDB or one in memory
//! (`store.rs`).
//!
//!     dcb-runtime-host serve <model.dcb.json>                      an HTTP API for the model's commands
//!     dcb-runtime-host run <model.dcb.json> <command> <args-json>  decide one command, print the outcome
//!
//! The model is a file, an http(s):// URL fetched once on start, or a playground link
//! (`https://dcb.events/playground/#model=…` or `…#url=…`).
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
    /// What is logged to stderr: off, error, warn, info (every request), debug (plus the bodies, and every
    /// call to the store – its query, its append condition) or trace; or a filter such as `info,store=debug` (targets `http`, `store`).
    #[arg(long, env = "DCB_LOG", default_value = "info")]
    log: String,
    /// How each log line is written: text for a person, json – one object per line – for a log collector.
    #[arg(long, env = "DCB_LOG_FORMAT", value_enum, default_value_t = LogFormat::Text)]
    log_format: LogFormat,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

/// A bare level is this host's: its libraries – wasmtime, the store
/// clients – log their warnings only. Text is coloured on a terminal
/// only, and never with `NO_COLOR` set.
fn init_log(log: &str, format: LogFormat) -> Result<()> {
    use std::io::IsTerminal;
    let levels = ["off", "error", "warn", "info", "debug", "trace"];
    let directives = if levels.contains(&log) { format!("warn,dcb_runtime_host={log},http={log},store={log}") } else { log.to_string() };
    let filter = tracing_subscriber::EnvFilter::try_new(&directives).map_err(|e| format_err!("--log {log}: {e}"))?;
    let log = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr);
    match format {
        LogFormat::Text => {
            let colour = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
            log.with_target(false).with_ansi(colour).init();
        }
        LogFormat::Json => log.json().flatten_event(true).with_span_list(false).init(),
    }
    Ok(())
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
        /// The model, as the playground exports it (JSON): a file, an http(s):// URL to fetch it from, or a
        /// playground link (…#model=… or …#url=…).
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
        /// Serve GET /projections/{Projection}: a projection's value, read by its tags and arguments.
        #[arg(long, env = "DCB_WITH_PROJECTIONS")]
        with_projections: bool,
    },
    /// Decide one command and print the outcome as JSON.
    Run {
        /// The model, as the playground exports it (JSON): a file, an http(s):// URL to fetch it from, or a
        /// playground link (…#model=… or …#url=…).
        model: String,
        #[command(flatten)]
        common: Common,
        /// The command's name.
        command: String,
        /// Its arguments, as a JSON object.
        args: String,
    },
}

/// The model's JSON and the name to log it by: from a file, fetched from an http(s):// URL, or out of
/// a playground link – packed into it (`#model=<gzipped, base64url-encoded JSON>`) or the address of
/// a file holding one (`#url=<URI-encoded address>`). Whatever follows an `&` in a link is for the
/// playground's interface, and ignored.
async fn load_model(source: &str) -> Result<(String, String)> {
    if !(source.starts_with("http://") || source.starts_with("https://")) {
        let text = std::fs::read_to_string(source).map_err(|e| format_err!("reading {source}: {e}"))?;
        return Ok((text, source.to_string()));
    }
    let fragment = source.split_once('#').map_or("", |(_, fragment)| fragment);
    let value = |prefix: &str| fragment.strip_prefix(prefix).map(|rest| rest.split('&').next().unwrap_or(""));
    if let Some(packed) = value("model=") {
        let text = unpack(packed).map_err(|e| format_err!("unpacking the model in {}: {e}", shorten(source)))?;
        let name = serde_json::from_str::<serde_json::Value>(&text)?["name"].as_str().map(str::to_string);
        return Ok((text, name.map_or_else(|| shorten(source), |name| format!("playground model \"{name}\""))));
    }
    let address = match value("url=") {
        Some(encoded) => percent_encoding::percent_decode_str(encoded).decode_utf8_lossy().into_owned(),
        None => source.to_string(),
    };
    Ok((fetch(&address).await?, source.to_string()))
}

/// What a link's model may unpack to, and how long the packed form may be – as in the playground.
const UNPACKED_MAX_BYTES: u64 = 5 * 1024 * 1024;
const PACKED_MAX_LENGTH: usize = 1000 * 1000;

fn unpack(packed: &str) -> std::result::Result<String, String> {
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    use std::io::Read;
    if packed.len() > PACKED_MAX_LENGTH {
        return Err(format!("over {PACKED_MAX_LENGTH} characters, far more than any model"));
    }
    let config = GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
    let bytes = base64::Engine::decode(&GeneralPurpose::new(&base64::alphabet::URL_SAFE, config), packed)
        .map_err(|e| format!("no base64url: {e}"))?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(bytes.as_slice())
        .take(UNPACKED_MAX_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("no gzipped text: {e}"))?;
    if text.len() as u64 > UNPACKED_MAX_BYTES {
        return Err(format!("it unpacks to more than {} MB, far more than any model", UNPACKED_MAX_BYTES / 1024 / 1024));
    }
    serde_json::from_str::<serde_json::Value>(&text).map_err(|e| format!("no JSON: {e}"))?;
    Ok(text)
}

/// A link, cut to a length fit for a log line.
fn shorten(link: &str) -> String {
    match link.char_indices().nth(60) {
        Some((end, _)) => format!("{}…", &link[..end]),
        None => link.to_string(),
    }
}

async fn fetch(source: &str) -> Result<String> {
    let fail = |e: String| format_err!("fetching {source}: {e}");
    let response = reqwest::get(source).await.map_err(|e| fail(e.to_string()))?;
    if !response.status().is_success() {
        return Err(fail(response.status().to_string()));
    }
    let text = response.text().await.map_err(|e| fail(e.to_string()))?;
    serde_json::from_str::<serde_json::Value>(&text).map_err(|e| fail(format!("no JSON: {e}")))?;
    Ok(text)
}

async fn pool(model: String, common: &Common, size: usize) -> Result<Pool> {
    let store = EventStore::connect(&common.store, common.store_timeout).await?;
    Pool::new(common.wasm.as_deref(), model, store, size).await
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve { model, common, listen, instances, with_docs, with_read, with_projections } => {
            init_log(&common.log, common.log_format)?;
            let size = instances.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
            let (model, name) = load_model(&model).await?;
            let api = Arc::new(http::Api::new(pool(model, &common, size).await?, with_read, with_projections).await?);
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            tracing::info!("serving {name} on http://{listen}{}", if with_docs { " (docs at /docs)" } else { "" });
            if common.store.starts_with("memory:") {
                tracing::warn!("the events are kept in memory – gone when the host stops");
            }
            if !listener.local_addr()?.ip().is_loopback() {
                tracing::warn!(
                    "listening beyond this machine. This API is the core of your application, not its edge: \
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
            init_log(&common.log, common.log_format)?;
            let (model, _) = load_model(&model).await?;
            let outcome = pool(model, &common, 1).await?.handle(&command, &args).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `{"name":"Link test","dcbModelVersion":1,"sandbox":{"steps":[]}}`, packed as the playground packs it.
    const PACKED: &str = "H4sIAAAAAAAAE6tWykvMTVWyUvLJzMtWKEktLlHSUUpJTvLNT0nNCUstKs7Mz1OyMtRRKk7MS0nKr1CyqlYqLkktKFayio6trQUAFm_x7j8AAAA";

    #[tokio::test]
    async fn unpacks_a_playground_link() {
        let (text, name) = load_model(&format!("https://dcb.events/playground/#model={PACKED}&surface=code&experimental"))
            .await
            .unwrap();
        assert_eq!(text, r#"{"name":"Link test","dcbModelVersion":1,"sandbox":{"steps":[]}}"#);
        assert_eq!(name, r#"playground model "Link test""#);
    }

    #[tokio::test]
    async fn refuses_a_link_that_does_not_unpack() {
        let error = load_model("https://dcb.events/playground/#model=H4sI").await.unwrap_err();
        assert!(error.to_string().starts_with("unpacking the model in https://dcb.events/playground/#model=H4sI:"));
    }

    #[test]
    fn refuses_a_model_that_unpacks_too_large() {
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gzip.write_all(&vec![b' '; UNPACKED_MAX_BYTES as usize + 1]).unwrap();
        let packed = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, gzip.finish().unwrap());
        assert!(unpack(&packed).unwrap_err().contains("more than 5 MB"));
    }
}
