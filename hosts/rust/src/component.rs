//! The runtime component, its `store` import implemented over UmaDB, and
//! a pool of instances with the model loaded.
//!
//! An instance decides one command at a time, so concurrent requests
//! each take one from the pool; concurrent decisions need no lock among
//! themselves – the append condition is the lock. The bindings are
//! async: the guest calls `read` as a blocking function, and wasmtime
//! suspends it while the host awaits the store.

use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;
use umadb_client::AsyncUmaDbClient;
use umadb_dcb::{DcbAppendCondition, DcbError, DcbEvent, DcbEventStoreAsync, DcbQuery, DcbQueryItem};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Result, Store, format_err};

wasmtime::component::bindgen!({
    path: "../../wit/runtime.wit",
    world: "runtime",
    imports: { default: async | trappable },
    exports: { default: async },
});

pub use dcb::runtime::types::Event;
pub use exports::dcb::runtime::decider::Outcome;
use dcb::runtime::types::{AppendCondition, AppendError, QueryItem, SequencedEvent};

pub struct Host {
    client: Arc<AsyncUmaDbClient>,
}

fn query_of(items: Vec<QueryItem>) -> DcbQuery {
    items.into_iter().fold(DcbQuery::new(), |query, item| {
        query.item(DcbQueryItem::new().types(item.types).tags(item.tags))
    })
}

impl dcb::runtime::types::Host for Host {}

impl dcb::runtime::store::Host for Host {
    async fn head(&mut self) -> Result<std::result::Result<Option<u64>, String>> {
        Ok(self.client.head().await.map_err(|e| e.to_string()))
    }

    async fn read(&mut self, query: Vec<QueryItem>) -> Result<std::result::Result<Vec<SequencedEvent>, String>> {
        Ok(self
            .client
            .read_with_head(Some(query_of(query)), None, false, None)
            .await
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

    async fn append(
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
        Ok(match self.client.append(events, condition, None).await {
            Ok(position) => Ok(position),
            Err(DcbError::IntegrityError(_)) => Err(AppendError::Integrity),
            Err(e) => Err(AppendError::Other(e.to_string())),
        })
    }
}

struct Instance {
    store: Store<Host>,
    runtime: Runtime,
}

pub struct Pool {
    engine: Engine,
    component: Component,
    linker: Linker<Host>,
    client: Arc<AsyncUmaDbClient>,
    model: String,
    idle: Mutex<Vec<Instance>>,
    permits: Semaphore,
}

/// Compiling the component to native code takes seconds – it carries a
/// whole JavaScript engine – so the result is kept beside it as
/// `<wasm>.cwasm` and loaded from there while it is newer than the
/// component. Only this host writes that file, for this engine.
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
    // A cache that cannot be written only costs the next start its time.
    let _ = std::fs::write(&cached, &compiled);
    // Safety: just compiled by this engine.
    unsafe { Component::deserialize(engine, &compiled) }
}

impl Pool {
    /// Loads the component and the model, and keeps `size` instances.
    /// The first is made here, so a model that does not load fails the
    /// start rather than the first request.
    pub async fn new(wasm: &str, model: String, client: AsyncUmaDbClient, size: usize) -> Result<Self> {
        let engine = Engine::new(&Config::new())?;
        let component = load_component(&engine, wasm)?;
        let mut linker = Linker::new(&engine);
        Runtime::add_to_linker::<Host, HasSelf<Host>>(&mut linker, |host| host)?;
        let pool = Self {
            engine,
            component,
            linker,
            client: Arc::new(client),
            model,
            idle: Mutex::new(Vec::new()),
            permits: Semaphore::new(size.max(1)),
        };
        let first = pool.instance().await?;
        pool.idle.lock().unwrap().push(first);
        Ok(pool)
    }

    async fn instance(&self) -> Result<Instance> {
        let mut store = Store::new(&self.engine, Host { client: self.client.clone() });
        let runtime = Runtime::instantiate_async(&mut store, &self.component, &self.linker).await?;
        if let Err(message) = runtime.dcb_runtime_decider().call_init(&mut store, &self.model).await? {
            return Err(format_err!("the model does not load: {message}"));
        }
        Ok(Instance { store, runtime })
    }

    /// Runs `f` on an idle instance, making one when none is. An
    /// instance whose call trapped is dropped: its state is undefined.
    async fn with_instance<T>(
        &self,
        f: impl AsyncFnOnce(&mut Store<Host>, &Runtime) -> Result<T>,
    ) -> Result<T> {
        let _permit = self.permits.acquire().await?;
        let idle = self.idle.lock().unwrap().pop();
        let mut instance = match idle {
            Some(instance) => instance,
            None => self.instance().await?,
        };
        let result = f(&mut instance.store, &instance.runtime).await;
        if result.is_ok() {
            self.idle.lock().unwrap().push(instance);
        }
        result
    }

    pub async fn handle(&self, command: &str, args: &str) -> Result<Outcome> {
        self.with_instance(async |store, runtime| runtime.dcb_runtime_decider().call_handle(store, command, args).await)
            .await
    }

    pub async fn contract(&self) -> Result<String> {
        self.with_instance(async |store, runtime| {
            runtime
                .dcb_runtime_decider()
                .call_contract(store)
                .await?
                .map_err(|message| format_err!("no contract: {message}"))
        })
        .await
    }

    pub fn client(&self) -> &AsyncUmaDbClient {
        &self.client
    }
}
