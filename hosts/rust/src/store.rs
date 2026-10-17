//! The event stores the host can connect the component's `store` import
//! to, picked by the store URL's scheme:
//!
//!   umadb://host:port      UmaDB (gRPC)            feature `umadb`
//!   umadb+tls://host:port  UmaDB over TLS
//!   tephra://host:port     Tephra (TCP protocol)   feature `tephra`
//!   postgres://user:pw@host:port/db
//!                          PostgreSQL              feature `postgres`
//!                          (also postgresql://)
//!   axonserver://[token@]host:port[/context]
//!                          Axon Server (gRPC)      feature `axonserver`
//!   axonserver+tls://…     Axon Server over TLS
//!   eventsourcingdb://token@host:port[/subject][?type-prefix=com.example]
//!                          EventSourcingDB (HTTP)  feature `eventsourcingdb`
//!   eventsourcingdb+tls://…  EventSourcingDB over HTTPS
//!   memory://              in this process, gone when it ends – to try
//!                          a model out, never for real data
//!
//! The scheme names the store, not the transport: two stores may well
//! speak the same one.
//!
//! UmaDB, Tephra and Axon Server speak the DCB specification's read and
//! conditional append, so their adapters only translate shapes: the
//! component's query items, events and append condition into the
//! client's, and its errors back – a failed condition as
//! `AppendError::Integrity`, which the component answers by deciding
//! again. EventSourcingDB has no tags, but checks an EventQL query on
//! append: its adapter keeps the tags in the event's data and writes the
//! condition as that query. PostgreSQL knows nothing of DCB: its adapter
//! keeps the events in a table of its own and checks the condition
//! itself, as does the in-memory store.
//!
//! Reads take `ReadOptions` – a cursor, a limit, a direction – passed
//! down to the store where it has them. Axon Server reads forwards only:
//! a backwards read there is refused (`unsupported`), not emulated.
//!
//! Every call is bounded by a timeout. A store that went away does not
//! always say so – a client may wait on a connection nobody will answer
//! – and a request waiting forever is worse than a 503.
//!
//! Every call is logged at `debug` – the query, the append condition,
//! what came back and how long it took; a failure at `warn`.

use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use wasmtime::{Result, format_err};

use crate::component::dcb::runtime::types::{AppendCondition, AppendError, Event, QueryItem, SequencedEvent};

/// The schemes this build connects to, for the error that names none of them.
fn schemes() -> String {
    let mut known = Vec::new();
    if cfg!(feature = "umadb") {
        known.push("umadb:// or umadb+tls:// for UmaDB");
    }
    if cfg!(feature = "tephra") {
        known.push("tephra:// for Tephra");
    }
    if cfg!(feature = "postgres") {
        known.push("postgres:// or postgresql:// for PostgreSQL");
    }
    if cfg!(feature = "axonserver") {
        known.push("axonserver:// or axonserver+tls:// for Axon Server");
    }
    if cfg!(feature = "eventsourcingdb") {
        known.push("eventsourcingdb:// or eventsourcingdb+tls:// for EventSourcingDB");
    }
    known.push("memory:// to try a model out");
    format!("use {}", known.join(", "))
}

/// Where a read starts, which way it goes and how much it returns. The
/// default reads every matching event, oldest first – what the
/// component asks for.
#[derive(Clone, Copy, Default)]
pub struct ReadOptions {
    /// Exclusive: forwards, the events after this position; backwards,
    /// those before it.
    pub after: Option<u64>,
    pub limit: Option<u64>,
    /// Newest first.
    pub backwards: bool,
}

/// The query matching every event: one item naming no types and no tags.
fn everything() -> Vec<QueryItem> {
    vec![QueryItem { types: Vec::new(), tags: Vec::new() }]
}

pub struct EventStore {
    backend: Backend,
    timeout: Duration,
}

enum Backend {
    Memory(memory::Memory),
    #[cfg(feature = "umadb")]
    Umadb(umadb::Umadb),
    #[cfg(feature = "tephra")]
    Tephra(tephra::Tephra),
    #[cfg(feature = "postgres")]
    Postgres(postgres::Postgres),
    #[cfg(feature = "axonserver")]
    AxonServer(axonserver::AxonServer),
    #[cfg(feature = "eventsourcingdb")]
    EventSourcingDb(eventsourcingdb::EventSourcingDb),
}

impl EventStore {
    pub async fn connect(url: &str, timeout: Duration) -> Result<Self> {
        let Some((scheme, address)) = url.split_once("://") else {
            return Err(format_err!("not a store URL: {url} – {}", schemes()));
        };
        // PostgreSQL takes the whole URL; the in-memory store needs none.
        #[cfg(not(any(feature = "umadb", feature = "tephra", feature = "axonserver", feature = "eventsourcingdb")))]
        let _ = address;
        let backend = async {
            match scheme {
                "memory" => Ok(Backend::Memory(memory::Memory::default())),
                #[cfg(feature = "umadb")]
                "umadb" => Ok(Backend::Umadb(umadb::Umadb::connect(&format!("http://{address}")).await?)),
                #[cfg(feature = "umadb")]
                "umadb+tls" => Ok(Backend::Umadb(umadb::Umadb::connect(&format!("https://{address}")).await?)),
                #[cfg(feature = "tephra")]
                "tephra" => Ok(Backend::Tephra(tephra::Tephra::connect(address).await?)),
                #[cfg(feature = "postgres")]
                "postgres" | "postgresql" => Ok(Backend::Postgres(postgres::Postgres::connect(url).await?)),
                #[cfg(feature = "axonserver")]
                "axonserver" => Ok(Backend::AxonServer(axonserver::AxonServer::connect(address, false).await?)),
                #[cfg(feature = "axonserver")]
                "axonserver+tls" => Ok(Backend::AxonServer(axonserver::AxonServer::connect(address, true).await?)),
                #[cfg(feature = "eventsourcingdb")]
                "eventsourcingdb" => Ok(Backend::EventSourcingDb(eventsourcingdb::EventSourcingDb::connect(address, false).await?)),
                #[cfg(feature = "eventsourcingdb")]
                "eventsourcingdb+tls" => Ok(Backend::EventSourcingDb(eventsourcingdb::EventSourcingDb::connect(address, true).await?)),
                _ => Err(format_err!("no store this build connects to at {url} – {}", schemes())),
            }
        };
        let backend = tokio::time::timeout(timeout, backend)
            .await
            .map_err(|_| format_err!("connecting to {url}: no answer within {timeout:?}"))??;
        Ok(Self { backend, timeout })
    }

    /// The store's name, one per store whatever the scheme it was reached by.
    pub fn kind(&self) -> &'static str {
        match &self.backend {
            Backend::Memory(_) => "memory",
            #[cfg(feature = "umadb")]
            Backend::Umadb(_) => "umadb",
            #[cfg(feature = "tephra")]
            Backend::Tephra(_) => "tephra",
            #[cfg(feature = "postgres")]
            Backend::Postgres(_) => "postgres",
            #[cfg(feature = "axonserver")]
            Backend::AxonServer(_) => "axonserver",
            #[cfg(feature = "eventsourcingdb")]
            Backend::EventSourcingDb(_) => "eventsourcingdb",
        }
    }

    /// Every `kind` this build knows.
    pub fn kinds() -> Vec<&'static str> {
        let mut kinds = vec!["memory"];
        if cfg!(feature = "umadb") {
            kinds.push("umadb");
        }
        if cfg!(feature = "tephra") {
            kinds.push("tephra");
        }
        if cfg!(feature = "postgres") {
            kinds.push("postgres");
        }
        if cfg!(feature = "axonserver") {
            kinds.push("axonserver");
        }
        if cfg!(feature = "eventsourcingdb") {
            kinds.push("eventsourcingdb");
        }
        kinds
    }

    async fn bounded<T, E>(&self, what: &str, call: impl Future<Output = std::result::Result<T, E>>, late: impl FnOnce(String) -> E) -> std::result::Result<T, E> {
        match tokio::time::timeout(self.timeout, call).await {
            Ok(result) => result,
            Err(_) => Err(late(format!("{what}: no answer within {:?}", self.timeout))),
        }
    }

    pub async fn head(&self) -> std::result::Result<Option<u64>, String> {
        let call = async {
            match &self.backend {
                Backend::Memory(store) => store.head(),
                #[cfg(feature = "umadb")]
                Backend::Umadb(store) => store.head().await,
                #[cfg(feature = "tephra")]
                Backend::Tephra(store) => store.head().await,
                #[cfg(feature = "postgres")]
                Backend::Postgres(store) => store.head().await,
                #[cfg(feature = "axonserver")]
                Backend::AxonServer(store) => store.head().await,
                #[cfg(feature = "eventsourcingdb")]
                Backend::EventSourcingDb(store) => store.head().await,
            }
        };
        let started = Instant::now();
        let result = self.bounded("head", call, |m| m).await;
        match &result {
            Ok(head) => tracing::debug!(target: "store", head, duration_ms = ms(started), "head"),
            Err(reason) => tracing::warn!(target: "store", error = %reason, duration_ms = ms(started), "head failed"),
        }
        result
    }

    /// Why this store cannot read with `options`; none when it can.
    pub fn unsupported(&self, options: &ReadOptions) -> Option<String> {
        match &self.backend {
            #[cfg(feature = "axonserver")]
            Backend::AxonServer(_) if options.backwards => Some("Axon Server reads forwards only.".into()),
            _ => {
                let _ = options;
                None
            }
        }
    }

    /// The events matching `query` – every event when there is none.
    pub async fn read(&self, query: Option<Vec<QueryItem>>, options: ReadOptions) -> std::result::Result<Vec<SequencedEvent>, String> {
        if let Some(reason) = self.unsupported(&options) {
            return Err(reason);
        }
        let query_json = query.as_deref().map_or(Value::from("everything"), items_json);
        let call = async {
            match &self.backend {
                Backend::Memory(store) => store.read(query, options),
                #[cfg(feature = "umadb")]
                Backend::Umadb(store) => store.read(query, options).await,
                #[cfg(feature = "tephra")]
                Backend::Tephra(store) => store.read(query, options).await,
                #[cfg(feature = "postgres")]
                Backend::Postgres(store) => store.read(query, options).await,
                #[cfg(feature = "axonserver")]
                Backend::AxonServer(store) => store.read(query, options).await,
                #[cfg(feature = "eventsourcingdb")]
                Backend::EventSourcingDb(store) => store.read(query, options).await,
            }
        };
        let started = Instant::now();
        let result = self.bounded("read", call, |m| m).await;
        let (after, limit, backwards) = (options.after, options.limit, options.backwards);
        match &result {
            Ok(events) => tracing::debug!(
                target: "store",
                query = %query_json, after, limit, backwards,
                events = events.len(), last = events.last().map(|e| e.position), duration_ms = ms(started),
                "read"
            ),
            Err(reason) => tracing::warn!(
                target: "store",
                query = %query_json, after, limit, backwards, error = %reason, duration_ms = ms(started),
                "read failed"
            ),
        }
        result
    }

    /// A timed-out append may still have landed; the request reports a
    /// failed store, and a retry by the client is decided afresh – it
    /// sees the events if they did.
    pub async fn append(
        &self,
        events: Vec<Event>,
        condition: Option<AppendCondition>,
    ) -> std::result::Result<u64, AppendError> {
        let events_json = Value::Array(events.iter().map(|e| json!({ "type": e.event_type, "tags": e.tags })).collect());
        let condition_json = condition.as_ref().map_or(Value::Null, |c| {
            json!({ "failIfEventsMatch": items_json(&c.fail_if_events_match), "after": c.after })
        });
        let call = async {
            match &self.backend {
                Backend::Memory(store) => store.append(events, condition),
                #[cfg(feature = "umadb")]
                Backend::Umadb(store) => store.append(events, condition).await,
                #[cfg(feature = "tephra")]
                Backend::Tephra(store) => store.append(events, condition).await,
                #[cfg(feature = "postgres")]
                Backend::Postgres(store) => store.append(events, condition).await,
                #[cfg(feature = "axonserver")]
                Backend::AxonServer(store) => store.append(events, condition).await,
                #[cfg(feature = "eventsourcingdb")]
                Backend::EventSourcingDb(store) => store.append(events, condition).await,
            }
        };
        let started = Instant::now();
        let result = self.bounded("append", call, AppendError::Other).await;
        let (events, condition) = (&events_json, &condition_json);
        match &result {
            Ok(position) => tracing::debug!(
                target: "store", %events, %condition, position, duration_ms = ms(started), "append"
            ),
            Err(AppendError::Integrity) => tracing::debug!(
                target: "store", %events, %condition, duration_ms = ms(started), "append: condition failed"
            ),
            Err(AppendError::Other(reason)) => tracing::warn!(
                target: "store", %events, %condition, error = %reason, duration_ms = ms(started), "append failed"
            ),
        }
        result
    }

    /// Whether the store answers; the reason when it does not.
    pub async fn health(&self) -> std::result::Result<(), String> {
        let call = async {
            match &self.backend {
                Backend::Memory(store) => store.health(),
                #[cfg(feature = "umadb")]
                Backend::Umadb(store) => store.health().await,
                #[cfg(feature = "tephra")]
                Backend::Tephra(store) => store.health().await,
                #[cfg(feature = "postgres")]
                Backend::Postgres(store) => store.health().await,
                #[cfg(feature = "axonserver")]
                Backend::AxonServer(store) => store.health().await,
                #[cfg(feature = "eventsourcingdb")]
                Backend::EventSourcingDb(store) => store.health().await,
            }
        };
        let started = Instant::now();
        let result = self.bounded("health", call, |m| m).await;
        match &result {
            Ok(()) => tracing::debug!(target: "store", duration_ms = ms(started), "health"),
            Err(reason) => tracing::warn!(target: "store", error = %reason, duration_ms = ms(started), "health failed"),
        }
        result
    }
}

/// Query items as the DCB specification spells them, for the log.
fn items_json(items: &[QueryItem]) -> Value {
    Value::Array(items.iter().map(|item| json!({ "types": item.types, "tags": item.tags })).collect())
}

/// Milliseconds since `started`, to the microsecond, for the log.
pub fn ms(started: Instant) -> f64 {
    started.elapsed().as_micros() as f64 / 1000.0
}

/// The events in a vector, behind a lock: a store to try a model out
/// with, nothing to start first. Gone when the process ends.
mod memory {
    use std::sync::Mutex;

    use super::{AppendCondition, AppendError, Event, QueryItem, ReadOptions, SequencedEvent, everything};

    #[derive(Default)]
    pub struct Memory(Mutex<Vec<Event>>);

    /// An item matches an event of one of its types (any, when it names
    /// none) carrying all its tags; no items match nothing.
    fn matches(items: &[QueryItem], event: &Event) -> bool {
        items.iter().any(|item| {
            (item.types.is_empty() || item.types.contains(&event.event_type))
                && item.tags.iter().all(|tag| event.tags.contains(tag))
        })
    }

    impl Memory {
        /// Positions start at 1: the event at index i is at position i + 1.
        pub fn head(&self) -> Result<Option<u64>, String> {
            let events = self.0.lock().map_err(|e| e.to_string())?;
            Ok((!events.is_empty()).then_some(events.len() as u64))
        }

        pub fn read(&self, query: Option<Vec<QueryItem>>, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
            let query = query.unwrap_or_else(everything);
            let events = self.0.lock().map_err(|e| e.to_string())?;
            let sequenced = events.iter().enumerate().map(|(i, event)| (i as u64 + 1, event));
            let in_range = |position: u64| match options.after {
                Some(after) if options.backwards => position < after,
                Some(after) => position > after,
                None => true,
            };
            let limit = options.limit.map_or(usize::MAX, |limit| usize::try_from(limit).unwrap_or(usize::MAX));
            let pick = |(position, event): (u64, &Event)| {
                (in_range(position) && matches(&query, event))
                    .then(|| SequencedEvent { position, event: event.clone() })
            };
            Ok(if options.backwards {
                sequenced.rev().filter_map(pick).take(limit).collect()
            } else {
                sequenced.filter_map(pick).take(limit).collect()
            })
        }

        pub fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
            let mut stored = self.0.lock().map_err(|e| AppendError::Other(e.to_string()))?;
            if let Some(condition) = condition {
                // No `after` is the whole log.
                let after = usize::try_from(condition.after.unwrap_or(0)).unwrap_or(usize::MAX);
                if stored.iter().skip(after).any(|event| matches(&condition.fail_if_events_match, event)) {
                    return Err(AppendError::Integrity);
                }
            }
            stored.extend(events);
            Ok(stored.len() as u64)
        }

        pub fn health(&self) -> Result<(), String> {
            Ok(())
        }
    }
}

#[cfg(feature = "umadb")]
mod umadb {
    use umadb_client::{AsyncUmaDbClient, ServingStatus};
    use umadb_dcb::{DcbAppendCondition, DcbError, DcbEvent, DcbEventStoreAsync, DcbQuery, DcbQueryItem};
    use wasmtime::{Result, format_err};

    use super::{AppendCondition, AppendError, Event, QueryItem, ReadOptions, SequencedEvent};

    pub struct Umadb(AsyncUmaDbClient);

    fn query_of(items: Vec<QueryItem>) -> DcbQuery {
        items.into_iter().fold(DcbQuery::new(), |query, item| {
            query.item(DcbQueryItem::new().types(item.types).tags(item.tags))
        })
    }

    impl Umadb {
        pub async fn connect(url: &str) -> Result<Self> {
            AsyncUmaDbClient::connect(url.to_string(), None, None, None)
                .await
                .map(Umadb)
                .map_err(|e| format_err!("connecting to UmaDB at {url}: {e}"))
        }

        pub async fn head(&self) -> Result<Option<u64>, String> {
            self.0.head().await.map_err(|e| e.to_string())
        }

        pub async fn read(&self, query: Option<Vec<QueryItem>>, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
            let limit = options.limit.map(|limit| u32::try_from(limit).unwrap_or(u32::MAX));
            // UmaDB starts at the position it is given; positions start at 1.
            let start = match options.after {
                Some(after) if options.backwards && after <= 1 => return Ok(Vec::new()),
                Some(after) if options.backwards => Some(after - 1),
                Some(after) => Some(after + 1),
                None => None,
            };
            let (events, _) = self
                .0
                .read_with_head(query.map(query_of), start, options.backwards, limit)
                .await
                .map_err(|e| e.to_string())?;
            Ok(events
                .into_iter()
                .map(|sequenced| SequencedEvent {
                    position: sequenced.position,
                    event: Event {
                        event_type: sequenced.event.event_type,
                        data: String::from_utf8_lossy(&sequenced.event.data).into_owned(),
                        tags: sequenced.event.tags,
                    },
                })
                .collect())
        }

        pub async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
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
            match self.0.append(events, condition, None).await {
                Ok(position) => Ok(position),
                Err(DcbError::IntegrityError(_)) => Err(AppendError::Integrity),
                Err(e) => Err(AppendError::Other(e.to_string())),
            }
        }

        pub async fn health(&self) -> Result<(), String> {
            match self.0.check_health("").await {
                Ok(ServingStatus::Serving) => Ok(()),
                Ok(status) => Err(format!("not serving: {status:?}")),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

#[cfg(feature = "tephra")]
mod tephra {
    use tephra_client::{
        AppendCondition as TephraCondition, AsyncClient, ClientError, ErrorCode, EventType, Position, Query,
        QueryItem as TephraItem, Tag, Tags,
    };
    use wasmtime::{Result, format_err};

    use super::{AppendCondition, AppendError, Event, QueryItem, ReadOptions, SequencedEvent};

    pub struct Tephra(AsyncClient);

    fn query_of(items: Vec<QueryItem>) -> Result<Query, String> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let types = item
                .types
                .into_iter()
                .map(|t| EventType::new(t).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            let tags = item
                .tags
                .into_iter()
                .map(|t| Tag::new(t).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            out.push(TephraItem::new(types, Tags::new(tags).map_err(|e| e.to_string())?));
        }
        Ok(Query::items(out))
    }

    impl Tephra {
        pub async fn connect(address: &str) -> Result<Self> {
            AsyncClient::connect(address)
                .await
                .map(Tephra)
                .map_err(|e| format_err!("connecting to Tephra at {address}: {e}"))
        }

        /// Positions are dense and start at 1, so the event count is the
        /// tip; an empty store has none.
        pub async fn head(&self) -> Result<Option<u64>, String> {
            let stats = self.0.stats().await.map_err(|e| e.to_string())?;
            Ok((stats.event_count > 0).then_some(stats.event_count))
        }

        pub async fn read(&self, query: Option<Vec<QueryItem>>, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
            let query = match query {
                Some(items) => query_of(items)?,
                None => Query::all(),
            };
            let read = if options.backwards {
                // Nothing to start before is the tip.
                let before = options.after.map_or(Position::MAX, Position::new);
                self.0.read_all_back(query, before, options.limit).await
            } else {
                self.0.read_all(query, Position::new(options.after.unwrap_or(0)), options.limit).await
            };
            let (events, _) = read.map_err(|e| e.to_string())?;
            Ok(events
                .into_iter()
                .map(|sequenced| {
                    let position = sequenced.position().get();
                    let event = sequenced.into_event();
                    SequencedEvent {
                        position,
                        event: Event {
                            event_type: event.event_type().to_string(),
                            data: String::from_utf8_lossy(event.payload()).into_owned(),
                            tags: event.tags().map(str::to_string).collect(),
                        },
                    }
                })
                .collect())
        }

        pub async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
            let events = events
                .into_iter()
                .map(|event| tephra_client::Event::new(event.event_type, event.tags, event.data.into_bytes()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| AppendError::Other(e.to_string()))?;
            let condition = match condition {
                // No `after` is the whole log – Tephra's `Position::ZERO`.
                Some(condition) => Some(
                    TephraCondition::new(query_of(condition.fail_if_events_match).map_err(AppendError::Other)?)
                        .after(Position::new(condition.after.unwrap_or(0))),
                ),
                None => None,
            };
            match self.0.append(events, condition).await {
                Ok(result) => Ok(result.last.get()),
                Err(ClientError::Server { code: ErrorCode::Conflict, .. }) => Err(AppendError::Integrity),
                Err(e) => Err(AppendError::Other(e.to_string())),
            }
        }

        pub async fn health(&self) -> Result<(), String> {
            self.0.stats().await.map(|_| ()).map_err(|e| e.to_string())
        }
    }
}

/// Events live in one table, `dcb_events`, created when missing; a
/// `search_path` in the URL puts it in a schema of its own. Positions
/// come from an identity column: a failed append may leave a gap, which
/// DCB allows.
///
/// An append takes a transaction-scoped advisory lock before it checks
/// the condition, so appends happen one at a time: no event can land
/// between the check and the insert, and none commits ahead of an
/// earlier position a reader could still be waiting for.
#[cfg(feature = "postgres")]
mod postgres {
    use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
    use tokio_postgres::config::SslMode;
    use tokio_postgres::types::ToSql;
    use tokio_postgres::{Config, NoTls};
    use wasmtime::{Result, format_err};

    use super::{AppendCondition, AppendError, Event, QueryItem, ReadOptions, SequencedEvent, everything};

    /// The advisory lock appends take – "dcb_evts" in ASCII. Stores in
    /// other schemas of the same database share it: they wait for one
    /// another, which is slower but no less correct.
    const APPEND_LOCK: i64 = 0x6463_625f_6576_7473;

    const SCHEMA: &str = "
        CREATE TABLE IF NOT EXISTS dcb_events (
            position BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            type TEXT NOT NULL,
            data TEXT NOT NULL,
            tags TEXT[] NOT NULL,
            recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE INDEX IF NOT EXISTS dcb_events_type ON dcb_events (type);
        CREATE INDEX IF NOT EXISTS dcb_events_tags ON dcb_events USING gin (tags);
    ";

    pub struct Postgres(Pool);

    type Params = Vec<Box<dyn ToSql + Sync + Send>>;

    /// The query as a condition on a row, its values appended to
    /// `params`. An item matches when the event's type is one of its
    /// types (any, when it names none) and the event carries all its
    /// tags; no items match nothing.
    fn matching(items: Vec<QueryItem>, params: &mut Params) -> String {
        if items.is_empty() {
            return "FALSE".into();
        }
        let items: Vec<String> = items
            .into_iter()
            .map(|item| {
                let mut clauses = Vec::new();
                if !item.types.is_empty() {
                    params.push(Box::new(item.types));
                    clauses.push(format!("type = ANY(${})", params.len()));
                }
                if !item.tags.is_empty() {
                    params.push(Box::new(item.tags));
                    clauses.push(format!("tags @> ${}", params.len()));
                }
                if clauses.is_empty() { "TRUE".into() } else { format!("({})", clauses.join(" AND ")) }
            })
            .collect();
        items.join(" OR ")
    }

    fn refs(params: &Params) -> Vec<&(dyn ToSql + Sync)> {
        params.iter().map(|p| p.as_ref() as &(dyn ToSql + Sync)).collect()
    }

    impl Postgres {
        /// Connects once, so a wrong URL or an absent server fails now,
        /// and creates the table when it is missing – under the append
        /// lock, as two hosts starting at once would race to.
        pub async fn connect(url: &str) -> Result<Self> {
            let config: Config = url.parse().map_err(|e| format_err!("not a PostgreSQL URL: {url}: {e}"))?;
            if config.get_ssl_mode() == SslMode::Require {
                return Err(format_err!("sslmode=require at {url}: TLS to PostgreSQL is not supported yet"));
            }
            let manager = Manager::from_config(config, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
            let pool = Pool::builder(manager).build()?;
            let fail = |e: String| format_err!("connecting to PostgreSQL at {url}: {e}");
            let mut client = pool.get().await.map_err(|e| fail(e.to_string()))?;
            let tx = client.transaction().await.map_err(|e| fail(e.to_string()))?;
            tx.execute("SELECT pg_advisory_xact_lock($1)", &[&APPEND_LOCK]).await.map_err(|e| fail(e.to_string()))?;
            tx.batch_execute(SCHEMA).await.map_err(|e| fail(e.to_string()))?;
            tx.commit().await.map_err(|e| fail(e.to_string()))?;
            drop(client);
            Ok(Postgres(pool))
        }

        pub async fn head(&self) -> Result<Option<u64>, String> {
            let client = self.0.get().await.map_err(|e| e.to_string())?;
            let row = client.query_one("SELECT max(position) FROM dcb_events", &[]).await.map_err(|e| e.to_string())?;
            Ok(row.get::<_, Option<i64>>(0).map(|p| p as u64))
        }

        pub async fn read(&self, query: Option<Vec<QueryItem>>, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
            let mut params = Params::new();
            let mut condition = format!("({})", matching(query.unwrap_or_else(everything), &mut params));
            if let Some(after) = options.after {
                params.push(Box::new(after as i64));
                condition += &format!(" AND position {} ${}", if options.backwards { "<" } else { ">" }, params.len());
            }
            let mut sql = format!(
                "SELECT position, type, data, tags FROM dcb_events WHERE {condition} ORDER BY position{}",
                if options.backwards { " DESC" } else { "" }
            );
            if let Some(limit) = options.limit {
                params.push(Box::new(i64::try_from(limit).unwrap_or(i64::MAX)));
                sql += &format!(" LIMIT ${}", params.len());
            }
            let client = self.0.get().await.map_err(|e| e.to_string())?;
            let rows = client
                .query(&sql, &refs(&params))
                .await
                .map_err(|e| e.to_string())?;
            Ok(rows
                .into_iter()
                .map(|row| SequencedEvent {
                    position: row.get::<_, i64>(0) as u64,
                    event: Event { event_type: row.get(1), data: row.get(2), tags: row.get(3) },
                })
                .collect())
        }

        pub async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
            let other = |e: String| AppendError::Other(e);
            let mut client = self.0.get().await.map_err(|e| other(e.to_string()))?;
            let tx = client.transaction().await.map_err(|e| other(e.to_string()))?;
            tx.execute("SELECT pg_advisory_xact_lock($1)", &[&APPEND_LOCK]).await.map_err(|e| other(e.to_string()))?;
            if let Some(condition) = condition {
                // No `after` is the whole log.
                let mut params: Params = vec![Box::new(condition.after.unwrap_or(0) as i64)];
                let matching = matching(condition.fail_if_events_match, &mut params);
                let row = tx
                    .query_one(&format!("SELECT EXISTS (SELECT 1 FROM dcb_events WHERE position > $1 AND ({matching}))"), &refs(&params))
                    .await
                    .map_err(|e| other(e.to_string()))?;
                if row.get::<_, bool>(0) {
                    // Dropping the transaction rolls it back.
                    return Err(AppendError::Integrity);
                }
            }
            let mut params = Params::new();
            let mut rows = Vec::with_capacity(events.len());
            for event in events {
                let n = params.len();
                rows.push(format!("(${}, ${}, ${})", n + 1, n + 2, n + 3));
                params.push(Box::new(event.event_type));
                params.push(Box::new(event.data));
                params.push(Box::new(event.tags));
            }
            let last = if rows.is_empty() {
                tx.query_one("SELECT max(position) FROM dcb_events", &[]).await
            } else {
                let sql = format!("WITH appended AS (INSERT INTO dcb_events (type, data, tags) VALUES {} RETURNING position) SELECT max(position) FROM appended", rows.join(", "));
                tx.query_one(&sql, &refs(&params)).await
            }
            .map_err(|e| other(e.to_string()))?
            .get::<_, Option<i64>>(0)
            .unwrap_or(0);
            tx.commit().await.map_err(|e| other(e.to_string()))?;
            Ok(last as u64)
        }

        pub async fn health(&self) -> Result<(), String> {
            let client = self.0.get().await.map_err(|e| e.to_string())?;
            client.query_one("SELECT 1", &[]).await.map(|_| ()).map_err(|e| e.to_string())
        }
    }
}

/// What follows the scheme in a store URL: `[user@]host:port[/path][?query]`.
#[cfg(any(feature = "axonserver", feature = "eventsourcingdb"))]
struct Address<'a> {
    user: Option<&'a str>,
    host: &'a str,
    /// Without its leading slash; empty when there is none.
    path: &'a str,
    #[cfg_attr(not(feature = "eventsourcingdb"), allow(dead_code))]
    query: &'a str,
}

#[cfg(any(feature = "axonserver", feature = "eventsourcingdb"))]
impl<'a> Address<'a> {
    fn parse(address: &'a str) -> Self {
        let (rest, query) = address.split_once('?').unwrap_or((address, ""));
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (user, host) = match authority.rsplit_once('@') {
            Some((user, host)) => (Some(user), host),
            None => (None, authority),
        };
        Address { user, host, path: path.trim_end_matches('/'), query }
    }

    #[cfg_attr(not(feature = "eventsourcingdb"), allow(dead_code))]
    fn param(&self, name: &str) -> Option<&'a str> {
        self.query.split('&').find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    }
}

/// Axon Server's DCB event store, in a context created as one – the
/// standalone server's with `axoniq.axonserver.standalone-dcb=true`. The
/// URL's path names the context, `default` when it names none; its user is
/// the access token, when access control is on.
///
/// Axon numbers events from sequence 0; positions here start at 1, as in
/// the other stores, so a position is its sequence + 1. Axon's head – the
/// sequence the next event gets – is then the position of the last.
///
/// A tag `key:value` is Axon's tag of that key and value, as Axon
/// Framework writes them; a tag without a colon has an empty value. The
/// events Axon sources carry no tags – their tags are a call per event
/// away – so a read returns events without them: the component folds
/// types and data only.
///
/// Axon sources forwards only, so a backwards read is refused; a limit
/// stops taking events from the stream, which cancels the rest.
#[cfg(feature = "axonserver")]
mod axonserver {
    use std::time::{SystemTime, UNIX_EPOCH};

    use tonic::metadata::{AsciiMetadataValue, MetadataValue};
    use tonic::service::Interceptor;
    use tonic::service::interceptor::InterceptedService;
    use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
    use tonic::{Code, Request, Status};
    use wasmtime::{Result, format_err};

    use super::{Address, AppendCondition, AppendError, Event, QueryItem, ReadOptions, SequencedEvent};

    /// The generated code refers across packages by relative paths, so
    /// the modules nest as the packages do. Of `common.proto` only what
    /// `dcb.proto` uses is used.
    #[allow(dead_code, clippy::all)]
    mod io {
        pub mod axoniq {
            pub mod axonserver {
                pub mod grpc {
                    tonic::include_proto!("io.axoniq.axonserver.grpc");
                    pub mod event {
                        pub mod dcb {
                            tonic::include_proto!("io.axoniq.axonserver.grpc.event.dcb");
                        }
                    }
                }
            }
        }
    }
    use io::axoniq::axonserver::grpc::event::dcb::{
        self as proto, AppendEventsRequest, ConsistencyCondition, Criterion, GetHeadRequest, SourceEventsRequest,
        TagsAndNamesCriterion, TaggedEvent, dcb_event_store_client::DcbEventStoreClient,
        source_events_response::Result as Sourced,
    };

    /// Names the context, and the token when there is one, on every call.
    #[derive(Clone)]
    pub struct Headers {
        context: AsciiMetadataValue,
        token: Option<AsciiMetadataValue>,
    }

    impl Interceptor for Headers {
        fn call(&mut self, mut request: Request<()>) -> std::result::Result<Request<()>, Status> {
            request.metadata_mut().insert("axoniq-context", self.context.clone());
            if let Some(token) = &self.token {
                request.metadata_mut().insert("axoniq-access-token", token.clone());
            }
            Ok(request)
        }
    }

    pub struct AxonServer(DcbEventStoreClient<InterceptedService<Channel, Headers>>);

    fn text(status: Status) -> String {
        format!("{:?}: {}", status.code(), status.message())
    }

    fn tag_of(tag: String) -> proto::Tag {
        let (key, value) = tag.split_once(':').unwrap_or((&tag, ""));
        proto::Tag { key: key.as_bytes().to_vec(), value: value.as_bytes().to_vec() }
    }

    fn criteria_of(items: Vec<QueryItem>) -> Vec<Criterion> {
        items
            .into_iter()
            .map(|item| Criterion {
                tags_and_names: Some(TagsAndNamesCriterion {
                    name: item.types,
                    tag: item.tags.into_iter().map(tag_of).collect(),
                }),
            })
            .collect()
    }

    impl AxonServer {
        pub async fn connect(address: &str, tls: bool) -> Result<Self> {
            let address = Address::parse(address);
            let fail = |e: String| format_err!("connecting to Axon Server at {}: {e}", address.host);
            let header = |value: &str, what: &str| {
                MetadataValue::try_from(value).map_err(|_| fail(format!("{what} is no valid header value")))
            };
            let headers = Headers {
                context: header(if address.path.is_empty() { "default" } else { address.path }, "the context")?,
                token: address.user.map(|token| header(token, "the token")).transpose()?,
            };
            let mut endpoint = Endpoint::from_shared(format!("{}://{}", if tls { "https" } else { "http" }, address.host))
                .map_err(|e| fail(e.to_string()))?;
            if tls {
                endpoint = endpoint.tls_config(ClientTlsConfig::new().with_native_roots()).map_err(|e| fail(e.to_string()))?;
            }
            let channel = endpoint.connect().await.map_err(|e| fail(e.to_string()))?;
            let store = AxonServer(DcbEventStoreClient::with_interceptor(channel, headers));
            // A context that is none, or no DCB one, fails here, not on the
            // first command.
            store.head().await.map_err(fail)?;
            Ok(store)
        }

        pub async fn head(&self) -> Result<Option<u64>, String> {
            let head = self.0.clone().get_head(GetHeadRequest {}).await.map_err(text)?.into_inner().sequence;
            Ok((head > 0).then_some(head as u64))
        }

        /// Backwards reads are refused before they get here.
        pub async fn read(&self, query: Option<Vec<QueryItem>>, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
            // No items match nothing; Axon's empty criteria match all.
            let criterion = match query {
                Some(items) if items.is_empty() => return Ok(Vec::new()),
                Some(items) => criteria_of(items),
                None => Vec::new(),
            };
            // The events after a position start at its sequence.
            let request = SourceEventsRequest { from_sequence: options.after.unwrap_or(0) as i64, criterion };
            let mut stream = self.0.clone().source(request).await.map_err(text)?.into_inner();
            let limit = options.limit.map_or(usize::MAX, |limit| usize::try_from(limit).unwrap_or(usize::MAX));
            let mut events = Vec::new();
            while events.len() < limit
                && let Some(response) = stream.message().await.map_err(text)?
            {
                if let Some(Sourced::Event(proto::SequencedEvent { sequence, event: Some(event) })) = response.result {
                    events.push(SequencedEvent {
                        position: sequence as u64 + 1,
                        event: Event {
                            event_type: event.name,
                            data: String::from_utf8_lossy(&event.payload).into_owned(),
                            tags: Vec::new(),
                        },
                    });
                }
            }
            Ok(events)
        }

        pub async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
            let events = events
                .into_iter()
                .map(|event| TaggedEvent {
                    event: Some(proto::Event {
                        identifier: uuid::Uuid::new_v4().to_string(),
                        timestamp,
                        name: event.event_type,
                        version: String::new(),
                        payload: event.data.into_bytes(),
                        metadata: Default::default(),
                    }),
                    tag: event.tags.into_iter().map(tag_of).collect(),
                })
                .collect();
            // Axon rejects events from the marker's sequence on – the
            // position after `after`; no items match nothing.
            let condition = condition.filter(|c| !c.fail_if_events_match.is_empty()).map(|condition| ConsistencyCondition {
                consistency_marker: condition.after.unwrap_or(0) as i64,
                criterion: criteria_of(condition.fail_if_events_match),
            });
            let request = AppendEventsRequest { condition, event: events };
            match self.0.clone().append(tokio_stream::once(request)).await {
                Ok(response) => {
                    let response = response.into_inner();
                    Ok((response.sequence_of_the_first_event + i64::from(response.transaction_size)) as u64)
                }
                Err(status) if is_conflict(&status) => Err(AppendError::Integrity),
                Err(status) => Err(AppendError::Other(text(status))),
            }
        }

        pub async fn health(&self) -> Result<(), String> {
            self.head().await.map(|_| ())
        }
    }

    /// A failed consistency condition: Axon cancels the append, and names
    /// the exception only in the message – its error code is the generic
    /// AXONIQ-0001.
    fn is_conflict(status: &Status) -> bool {
        status.code() == Code::Cancelled && status.message().contains("ConsistencyConditionException")
    }
}

/// EventSourcingDB, over its HTTP API. The URL's user is the API token;
/// its path names the subject events are written to, `/` when it names
/// none.
///
/// EventSourcingDB knows no tags, and wants event types in reverse-domain
/// notation. So an event's type is the URL's `type-prefix` (`dcb.runtime`
/// when it names none) and its own, `dcb.runtime.CourseDefined`; its data
/// is `{"payload": …, "tags": […]}`. Reads and conditions are EventQL
/// queries over the events of that subject and prefix – those this
/// adapter writes.
///
/// Event ids count from 0; positions here start at 1, as in the other
/// stores, so a position is its id + 1.
#[cfg(feature = "eventsourcingdb")]
mod eventsourcingdb {
    use reqwest::{Client, Response, StatusCode};
    use serde_json::{Value, json};
    use wasmtime::{Result, format_err};

    use super::{Address, AppendCondition, AppendError, Event, QueryItem, ReadOptions, SequencedEvent, everything};

    /// The `source` of the events written: the CloudEvents attribute
    /// naming who wrote them.
    const SOURCE: &str = "urn:dcb-runtime";

    pub struct EventSourcingDb {
        client: Client,
        api: String,
        token: String,
        subject: String,
        /// The type prefix, with its trailing dot.
        prefix: String,
    }

    /// A string as an EventQL literal: EventQL reads JSON's escapes.
    fn literal(s: &str) -> String {
        Value::from(s).to_string()
    }

    impl EventSourcingDb {
        pub async fn connect(address: &str, tls: bool) -> Result<Self> {
            let address = Address::parse(address);
            let fail = |e: String| format_err!("connecting to EventSourcingDB at {}: {e}", address.host);
            let token = address.user.ok_or_else(|| fail("no API token – eventsourcingdb://token@host:port".into()))?;
            let store = EventSourcingDb {
                client: Client::new(),
                api: format!("{}://{}/api/v1", if tls { "https" } else { "http" }, address.host),
                token: token.to_string(),
                subject: format!("/{}", address.path),
                prefix: format!("{}.", address.param("type-prefix").unwrap_or("dcb.runtime")),
            };
            store.health().await.map_err(fail)?;
            Ok(store)
        }

        async fn post(&self, route: &str, body: Value) -> Result<Response, String> {
            self.client
                .post(format!("{}/{route}", self.api))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await
                .map_err(|e| e.to_string())
        }

        async fn failure(response: Response) -> String {
            let status = response.status();
            match response.text().await {
                Ok(body) if !body.trim().is_empty() => format!("{status}: {}", body.trim()),
                _ => status.to_string(),
            }
        }

        /// The query as an EventQL condition on `e`, the events this
        /// adapter writes. An item matches when the event's type is one of
        /// its types (any, when it names none) and its tags hold all the
        /// item's; no items match nothing.
        fn matching(&self, items: Vec<QueryItem>) -> String {
            let items: Vec<String> = items
                .into_iter()
                .map(|item| {
                    let mut clauses = Vec::new();
                    if !item.types.is_empty() {
                        let types: Vec<String> =
                            item.types.iter().map(|t| format!("e.type == {}", literal(&format!("{}{t}", self.prefix)))).collect();
                        clauses.push(format!("({})", types.join(" OR ")));
                    }
                    clauses.extend(item.tags.iter().map(|tag| format!("e.data.tags CONTAINS {}", literal(tag))));
                    if clauses.is_empty() { "TRUE".into() } else { format!("({})", clauses.join(" AND ")) }
                })
                .collect();
            // The tests on the subject and prefix come first: AND stops at
            // the first false, and other events may have no tags to test.
            format!(
                "e.subject == {} AND STARTSWITH(e.type, {}) AND ({})",
                literal(&self.subject),
                literal(&self.prefix),
                if items.is_empty() { "FALSE".into() } else { items.join(" OR ") }
            )
        }

        /// The events' order is the store's, as the ids'; the last event of
        /// all subjects is the first read backwards from the root.
        pub async fn head(&self) -> Result<Option<u64>, String> {
            let body = json!({"subject": "/", "options": {"recursive": true, "order": "antichronological"}});
            let mut response = self.post("read-events", body).await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let mut line = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
                line.extend_from_slice(&chunk);
                if let Some(end) = line.iter().position(|&b| b == b'\n') {
                    line.truncate(end);
                    break;
                }
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                return Ok(None);
            }
            match serde_json::from_slice::<Value>(&line).map_err(|e| e.to_string())? {
                item if item["type"] == "event" => Ok(Some(id_of(&item["payload"])? + 1)),
                item => Err(format!("unexpected answer: {item}")),
            }
        }

        pub async fn read(&self, query: Option<Vec<QueryItem>>, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
            let query = query.unwrap_or_else(everything);
            if query.is_empty() {
                return Ok(Vec::new());
            }
            // Position p is id p - 1: after it are the ids from p on,
            // before it those below p - 1.
            let after = match options.after {
                Some(after) if options.backwards => format!("(e.id AS INT) < {} AND ", after.saturating_sub(1)),
                Some(after) => format!("(e.id AS INT) >= {after} AND "),
                None => String::new(),
            };
            let order = if options.backwards { " DESC" } else { "" };
            let top = options.limit.map(|limit| format!(" TOP {limit}")).unwrap_or_default();
            let eventql = format!("FROM e IN events WHERE {after}{} ORDER BY e.id{order}{top} PROJECT INTO e", self.matching(query));
            let response = self.post("run-eventql-query", json!({"query": eventql})).await?;
            if !response.status().is_success() {
                return Err(Self::failure(response).await);
            }
            let body = response.text().await.map_err(|e| e.to_string())?;
            let mut events = Vec::new();
            for line in body.lines().filter(|l| !l.trim().is_empty()) {
                let item: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
                match item["type"].as_str() {
                    Some("row") => {
                        let event = &item["payload"];
                        let event_type = event["type"].as_str().unwrap_or_default();
                        events.push(SequencedEvent {
                            position: id_of(event)? + 1,
                            event: Event {
                                event_type: event_type.strip_prefix(&self.prefix).unwrap_or(event_type).to_string(),
                                data: event["data"]["payload"].to_string(),
                                tags: event["data"]["tags"]
                                    .as_array()
                                    .map(|tags| tags.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
                                    .unwrap_or_default(),
                            },
                        });
                    }
                    Some("error") => return Err(item["payload"]["error"].as_str().unwrap_or("query failed").to_string()),
                    _ => {}
                }
            }
            Ok(events)
        }

        pub async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
            let other = AppendError::Other;
            let events = events
                .into_iter()
                .map(|event| {
                    let payload: Value = serde_json::from_str(&event.data).map_err(|e| other(format!("data is no JSON: {e}")))?;
                    Ok(json!({
                        "source": SOURCE,
                        "subject": self.subject,
                        "type": format!("{}{}", self.prefix, event.event_type),
                        "data": {"payload": payload, "tags": event.tags},
                    }))
                })
                .collect::<Result<Vec<_>, AppendError>>()?;
            let mut body = json!({"events": events});
            if let Some(condition) = condition.filter(|c| !c.fail_if_events_match.is_empty()) {
                // The events after `after` are those from its id on; ids
                // compare as text in a precondition, unless cast.
                let after = condition.after.map(|after| format!("(e.id AS INT) >= {after} AND ")).unwrap_or_default();
                let eventql = format!(
                    "FROM e IN events WHERE {after}{} PROJECT INTO COUNT() == 0",
                    self.matching(condition.fail_if_events_match)
                );
                body["preconditions"] = json!([{"type": "isEventQlQueryTrue", "payload": {"query": eventql}}]);
            }
            let response = self.post("write-events", body).await.map_err(other)?;
            match response.status() {
                StatusCode::OK => {
                    let written: Value = response.json().await.map_err(|e| other(e.to_string()))?;
                    let last = written.as_array().and_then(|events| events.last()).ok_or_else(|| other("no events written".into()))?;
                    Ok(id_of(last).map_err(other)? + 1)
                }
                StatusCode::CONFLICT => Err(AppendError::Integrity),
                _ => Err(other(Self::failure(response).await)),
            }
        }

        pub async fn health(&self) -> Result<(), String> {
            let response = self.post("verify-api-token", json!({})).await?;
            if response.status().is_success() { Ok(()) } else { Err(Self::failure(response).await) }
        }
    }

    fn id_of(event: &Value) -> Result<u64, String> {
        event["id"].as_str().and_then(|id| id.parse().ok()).ok_or_else(|| format!("an event without id: {event}"))
    }
}
