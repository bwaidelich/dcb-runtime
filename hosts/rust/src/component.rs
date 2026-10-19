//! The runtime component, its `store` import implemented over whichever
//! event store the host connected to (`store.rs`), and a pool of
//! instances with the model loaded.
//!
//! An instance decides one command at a time, so concurrent requests
//! each take one from the pool; concurrent decisions need no lock among
//! themselves – the append condition is the lock. The component makes
//! one attempt; a lost race is decided again here, after a wait, since
//! a wait inside the component would hold its instance. The bindings are
//! async: the guest calls `read` as a blocking function, and wasmtime
//! suspends it while the host awaits the store.

use std::hash::{BuildHasher, DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Result, Store, format_err};

wasmtime::component::bindgen!({
    path: "../../wit/runtime.wit",
    world: "runtime",
    imports: { default: async | trappable },
    exports: { default: async },
});

pub use dcb::runtime::types::Event;
pub use exports::dcb::runtime::decider::{Outcome, ProjectionOutcome};
use dcb::runtime::types::{AppendCondition, AppendError, QueryItem, SequencedEvent};

use crate::store::{EventStore, ReadOptions};

pub struct Host {
    store: Arc<EventStore>,
}

impl dcb::runtime::types::Host for Host {}

impl dcb::runtime::store::Host for Host {
    async fn head(&mut self) -> Result<std::result::Result<Option<u64>, String>> {
        Ok(self.store.head().await)
    }

    async fn read(&mut self, query: Vec<QueryItem>) -> Result<std::result::Result<Vec<SequencedEvent>, String>> {
        Ok(self.store.read(Some(query), ReadOptions::default()).await)
    }

    async fn append(
        &mut self,
        events: Vec<Event>,
        condition: Option<AppendCondition>,
    ) -> Result<std::result::Result<u64, AppendError>> {
        Ok(self.store.append(events, condition).await)
    }
}

struct Instance {
    store: Store<Host>,
    runtime: Runtime,
}

/// How often a command is decided before a lost race is answered as a
/// conflict, and the longest first wait between two attempts; each wait
/// after may be twice as long as the one before.
const MAX_ATTEMPTS: u32 = 3;
const FIRST_BACKOFF: Duration = Duration::from_millis(10);

pub struct Pool {
    engine: Engine,
    component: Component,
    linker: Linker<Host>,
    store: Arc<EventStore>,
    model: String,
    idle: Mutex<Vec<Instance>>,
    permits: Semaphore,
}

/// The runtime component, as `npm run build` left it (see `build.rs`).
const EMBEDDED: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../build/dcb-runtime.wasm"));

/// Compiling the component to native code takes seconds – it carries a
/// whole JavaScript engine – so the result is cached: the embedded
/// component's in the user's cache directory, under a key of the
/// component and the engine; a component from a file's beside it, as
/// `<wasm>.cwasm`, while it is newer than the component. Only this host
/// writes those files, for this engine.
fn load_component(engine: &Engine, wasm: Option<&str>) -> Result<Component> {
    let Some(wasm) = wasm else {
        let cached = embedded_cache(engine);
        if let Some(cached) = &cached {
            // Safety: the file is one this host serialized for this engine.
            if let Ok(component) = unsafe { Component::deserialize_file(engine, cached) } {
                return Ok(component);
            }
        }
        return compile(engine, EMBEDDED, cached.as_deref());
    };
    let cached = PathBuf::from(format!("{wasm}.cwasm"));
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    if let (Some(source), Some(compiled)) = (modified(Path::new(wasm)), modified(&cached))
        && compiled > source
        // Safety: the file is one this host serialized for this engine.
        && let Ok(component) = unsafe { Component::deserialize_file(engine, &cached) }
    {
        return Ok(component);
    }
    let bytes = std::fs::read(wasm).map_err(|e| format_err!("loading {wasm}: {e}"))?;
    compile(engine, &bytes, Some(&cached))
}

fn compile(engine: &Engine, wasm: &[u8], cache: Option<&Path>) -> Result<Component> {
    let compiled = engine.precompile_component(wasm)?;
    // A cache that cannot be written only costs the next start its time.
    // Written aside and renamed, so a concurrent start never reads half of it.
    if let Some(cache) = cache {
        let partial = cache.with_extension(format!("cwasm.{}", std::process::id()));
        if std::fs::write(&partial, &compiled).is_ok() && std::fs::rename(&partial, cache).is_err() {
            let _ = std::fs::remove_file(&partial);
        }
    }
    // Safety: just compiled by this engine.
    unsafe { Component::deserialize(engine, &compiled) }
}

/// `<cache dir>/dcb-runtime/<hash>.cwasm`, the hash over the embedded
/// component and what the engine's native code depends on.
fn embedded_cache(engine: &Engine) -> Option<PathBuf> {
    let mut hasher = DefaultHasher::new();
    EMBEDDED.hash(&mut hasher);
    engine.precompile_compatibility_hash().hash(&mut hasher);
    let dir = cache_dir()?.join("dcb-runtime");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{:016x}.cwasm", hasher.finish())))
}

fn cache_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = var("XDG_CACHE_HOME") {
        return Some(dir);
    }
    if cfg!(windows) {
        return var("LOCALAPPDATA");
    }
    let home = var("HOME")?;
    Some(if cfg!(target_os = "macos") { home.join("Library/Caches") } else { home.join(".cache") })
}

impl Pool {
    /// Loads the component – the embedded one, or `wasm` – and the model, and keeps `size` instances.
    /// The first is made here, so a model that does not load fails the
    /// start rather than the first request.
    pub async fn new(wasm: Option<&str>, model: String, store: EventStore, size: usize) -> Result<Self> {
        let engine = Engine::new(&Config::new())?;
        let component = load_component(&engine, wasm)?;
        let mut linker = Linker::new(&engine);
        Runtime::add_to_linker::<Host, HasSelf<Host>>(&mut linker, |host| host)?;
        let pool = Self {
            engine,
            component,
            linker,
            store: Arc::new(store),
            model,
            idle: Mutex::new(Vec::new()),
            permits: Semaphore::new(size.max(1)),
        };
        let first = pool.instance().await?;
        pool.idle.lock().unwrap().push(first);
        Ok(pool)
    }

    async fn instance(&self) -> Result<Instance> {
        let mut store = Store::new(&self.engine, Host { store: self.store.clone() });
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
        retrying(
            || self.with_instance(async |store, runtime| runtime.dcb_runtime_decider().call_handle(store, command, args).await),
            |outcome| matches!(outcome, Outcome::Conflict),
            MAX_ATTEMPTS,
            FIRST_BACKOFF,
        )
        .await
    }

    pub async fn project(&self, projection: &str, params: &str) -> Result<ProjectionOutcome> {
        self.with_instance(async |store, runtime| {
            runtime.dcb_runtime_decider().call_project(store, projection, params).await
        })
        .await
    }

    pub async fn contract(&self, with_projections: bool) -> Result<String> {
        self.with_instance(async |store, runtime| {
            runtime
                .dcb_runtime_decider()
                .call_contract(store, with_projections)
                .await?
                .map_err(|message| format_err!("no contract: {message}"))
        })
        .await
    }

    pub fn store(&self) -> &EventStore {
        &self.store
    }
}

/// Runs `attempt` until it does not `lose`, at most `attempts` times.
/// Before the n-th rerun it waits a random while up to `first`·2ⁿ⁻¹ –
/// exponential backoff with full jitter, so writers that lost to each
/// other spread out instead of colliding again in step. The last
/// outcome stands, lost or not.
async fn retrying<T, F: Future<Output = Result<T>>>(
    mut attempt: impl FnMut() -> F,
    lose: impl Fn(&T) -> bool,
    attempts: u32,
    first: Duration,
) -> Result<T> {
    let mut made = 0;
    loop {
        let outcome = attempt().await?;
        made += 1;
        if made >= attempts || !lose(&outcome) {
            return Ok(outcome);
        }
        tokio::time::sleep(jittered(first * 2u32.pow(made - 1))).await;
    }
}

/// Somewhere between nothing and `max`. A fresh `RandomState` is seeded
/// at random, which is all the randomness a backoff needs.
fn jittered(max: Duration) -> Duration {
    let random = std::collections::hash_map::RandomState::new().hash_one(0u8);
    max.mul_f64((random % 10_000) as f64 / 10_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(losses: u32, attempts: u32) -> (bool, u32) {
        let mut made = 0;
        let lost = retrying(
            || {
                made += 1;
                let lost = made <= losses;
                async move { Ok(lost) }
            },
            |lost| *lost,
            attempts,
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        (lost, made)
    }

    #[tokio::test]
    async fn decides_again_until_it_wins() {
        assert_eq!(run(2, 3).await, (false, 3));
    }

    #[tokio::test]
    async fn stops_at_the_first_win() {
        assert_eq!(run(0, 3).await, (false, 1));
    }

    #[tokio::test]
    async fn reports_the_last_loss_when_every_attempt_lost() {
        assert_eq!(run(5, 3).await, (true, 3));
    }

    #[test]
    fn waits_no_longer_than_its_bound() {
        for _ in 0..100 {
            assert!(jittered(Duration::from_millis(10)) <= Duration::from_millis(10));
        }
    }
}
