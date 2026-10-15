//! A host for the DCB Runtime component: implements its `store` import
//! with the UmaDB client and decides one command per invocation.
//!
//!     dcb-runtime-host <model.json> <command> <args-json>
//!
//! Prints the outcome as JSON. `UMADB_URL` names the store
//! (default `http://localhost:50051`), `DCB_RUNTIME_WASM` the component
//! (default `build/dcb-runtime.wasm`).

use serde_json::{Value, json};
use umadb_client::SyncUmaDbClient;
use umadb_dcb::{DcbAppendCondition, DcbError, DcbEvent, DcbEventStoreSync, DcbQuery, DcbQueryItem};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Result, Store, bail, format_err};

wasmtime::component::bindgen!({
    path: "../../wit/runtime.wit",
    world: "runtime",
    imports: { default: trappable },
});

use dcb::runtime::types::{AppendCondition, AppendError, Event, QueryItem, SequencedEvent};
use exports::dcb::runtime::decider::Outcome;

struct Host {
    client: SyncUmaDbClient,
}

fn query_of(items: Vec<QueryItem>) -> DcbQuery {
    items.into_iter().fold(DcbQuery::new(), |query, item| {
        query.item(DcbQueryItem::new().types(item.types).tags(item.tags))
    })
}

impl dcb::runtime::types::Host for Host {}

impl dcb::runtime::store::Host for Host {
    fn head(&mut self) -> Result<Option<u64>> {
        self.client.head().map_err(|e| format_err!("head: {e}"))
    }

    fn read(&mut self, query: Vec<QueryItem>) -> Result<std::result::Result<Vec<SequencedEvent>, String>> {
        Ok(self
            .client
            .read_with_head(Some(query_of(query)), None, false, None)
            .map(|(events, _)| {
                events
                    .into_iter()
                    .map(|sequenced| SequencedEvent {
                        position: sequenced.position,
                        event: Event {
                            event_type: sequenced.event.event_type,
                            data: String::from_utf8_lossy(&sequenced.event.data).into_owned(),
                            tags: sequenced.event.tags,
                        },
                    })
                    .collect()
            })
            .map_err(|e| e.to_string()))
    }

    fn append(
        &mut self,
        events: Vec<Event>,
        condition: Option<AppendCondition>,
    ) -> Result<std::result::Result<u64, AppendError>> {
        let events = events
            .into_iter()
            .map(|event| {
                DcbEvent::default()
                    .event_type(event.event_type)
                    .tags(event.tags)
                    .data(event.data.into_bytes())
            })
            .collect();
        let condition = condition.map(|condition| {
            DcbAppendCondition::new(query_of(condition.fail_if_events_match)).after(condition.after)
        });
        Ok(match self.client.append(events, condition, None) {
            Ok(position) => Ok(position),
            Err(DcbError::IntegrityError(_)) => Err(AppendError::Integrity),
            Err(e) => Err(AppendError::Other(e.to_string())),
        })
    }
}

fn outcome_json(outcome: Outcome) -> Value {
    let events = |events: Vec<Event>| -> Vec<Value> {
        events
            .into_iter()
            .map(|e| {
                json!({
                    "type": e.event_type,
                    "data": serde_json::from_str::<Value>(&e.data).unwrap_or(Value::String(e.data)),
                    "tags": e.tags,
                })
            })
            .collect()
    };
    match outcome {
        Outcome::Published(p) => json!({ "outcome": "published", "events": events(p.events), "position": p.position }),
        Outcome::Rejected(r) => json!({ "outcome": "rejected", "rejection": r.rejection, "ruleIndex": r.rule_index }),
        Outcome::Conflict => json!({ "outcome": "conflict" }),
        Outcome::Error(message) => json!({ "outcome": "error", "message": message }),
    }
}

// Compiling the component to native code takes seconds – it carries a
// whole JavaScript engine – so the result is kept beside it as
// `<wasm>.cwasm` and loaded from there while it is newer than the
// component. Only this host writes that file, for this engine.
fn load_component(engine: &Engine, wasm: &str) -> Result<Component> {
    let cached = format!("{wasm}.cwasm");
    let modified = |path: &str| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    if let (Some(source), Some(compiled)) = (modified(wasm), modified(&cached)) {
        if compiled > source {
            // Safety: the file is one this host serialized for this engine.
            if let Ok(component) = unsafe { Component::deserialize_file(engine, &cached) } {
                return Ok(component);
            }
        }
    }
    let bytes = std::fs::read(wasm).map_err(|e| format_err!("loading {wasm}: {e}"))?;
    let compiled = engine.precompile_component(&bytes)?;
    // A cache that cannot be written only costs the next run its time.
    let _ = std::fs::write(&cached, &compiled);
    // Safety: just compiled by this engine.
    unsafe { Component::deserialize(engine, &compiled) }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, model_path, command, command_args] = args.as_slice() else {
        bail!("usage: dcb-runtime-host <model.json> <command> <args-json>");
    };
    let model = std::fs::read_to_string(model_path).map_err(|e| format_err!("reading {model_path}: {e}"))?;
    let url = std::env::var("UMADB_URL").unwrap_or_else(|_| "http://localhost:50051".into());
    let wasm = std::env::var("DCB_RUNTIME_WASM").unwrap_or_else(|_| "build/dcb-runtime.wasm".into());

    let client = SyncUmaDbClient::connect(url.clone(), None, None, None)
        .map_err(|e| format_err!("connecting to {url}: {e}"))?;

    let engine = Engine::new(&Config::new())?;
    let component = load_component(&engine, &wasm)?;
    let mut linker = Linker::new(&engine);
    Runtime::add_to_linker::<Host, HasSelf<Host>>(&mut linker, |host| host)?;
    let mut store = Store::new(&engine, Host { client });
    let runtime = Runtime::instantiate(&mut store, &component, &linker)?;
    let decider = runtime.dcb_runtime_decider();

    if let Err(message) = decider.call_init(&mut store, &model)? {
        bail!("the model does not load: {message}");
    }
    let outcome = decider.call_handle(&mut store, command, command_args)?;
    println!("{}", outcome_json(outcome));
    Ok(())
}
