//! The event stores the host can connect the component's `store` import
//! to, picked by the store URL's scheme:
//!
//!   memory://              in this process, gone when it ends – to try
//!                          a model out, never for real data
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
//!
//! The scheme names the store, not the transport: two stores may well
//! speak the same one.
//!
//! `EventStore` is what the rest of the host knows of a store. Below it,
//! each store is an `Adapter` in a file of its own, which names itself,
//! its schemes and what it cannot do in its `Kind`; adding a store is
//! adding that file and its line in `KINDS`. An adapter translates
//! shapes – the component's query items, events and append condition
//! into its client's, its errors back, a failed condition as
//! `AppendError::Integrity` – and positions: every store's start at 1
//! here, and `after` is exclusive.
//!
//! What DCB says of the edge cases is said here, once, not per adapter:
//! a query without items matches nothing, so it is answered without
//! asking the store; a condition without items fails on nothing, so it
//! is dropped. An adapter is never handed either.
//!
//! Reads take `ReadOptions` – a cursor, a limit, a direction – passed
//! down to the store. A store that cannot read backwards refuses that
//! (`unsupported`); it is not emulated.
//!
//! Every call is bounded by a timeout. A store that went away does not
//! always say so – a client may wait on a connection nobody will answer
//! – and a request waiting forever is worse than a 503.
//!
//! Every call is logged at `debug` – the query, the append condition,
//! what came back and how long it took; a failure at `warn`.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use wasmtime::{Result, format_err};

use crate::component::dcb::runtime::types::{AppendCondition, AppendError, Event, QueryItem, SequencedEvent};

/// Implements `Adapter` by the adapter's own async methods of the same
/// names and shapes. The in-memory store, alone, needs none.
#[cfg_attr(not(any(feature = "umadb", feature = "tephra", feature = "postgres", feature = "axonserver", feature = "eventsourcingdb")), allow(unused_macros))]
macro_rules! adapter {
    ($adapter:ty) => {
        impl $crate::store::Adapter for $adapter {
            fn head(&self) -> $crate::store::Answer<'_, Option<u64>> {
                Box::pin(<$adapter>::head(self))
            }

            fn read(
                &self,
                query: $crate::store::Query,
                options: $crate::store::ReadOptions,
            ) -> $crate::store::Answer<'_, Vec<$crate::store::SequencedEvent>> {
                Box::pin(<$adapter>::read(self, query, options))
            }

            fn append(
                &self,
                events: Vec<$crate::store::Event>,
                condition: Option<$crate::store::AppendCondition>,
            ) -> $crate::store::Answer<'_, u64, $crate::store::AppendError> {
                Box::pin(<$adapter>::append(self, events, condition))
            }

            fn health(&self) -> $crate::store::Answer<'_, ()> {
                Box::pin(<$adapter>::health(self))
            }
        }
    };
}

#[cfg(any(feature = "axonserver", feature = "eventsourcingdb"))]
mod address;
#[cfg(feature = "axonserver")]
mod axonserver;
#[cfg(feature = "eventsourcingdb")]
mod eventsourcingdb;
mod memory;
#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "tephra")]
mod tephra;
#[cfg(feature = "umadb")]
mod umadb;

#[cfg(test)]
mod conformance;

/// The stores this build connects to: the built-in one first, then the
/// DCB stores alphabetically.
static KINDS: &[Kind] = &[
    memory::KIND,
    #[cfg(feature = "axonserver")]
    axonserver::KIND,
    #[cfg(feature = "eventsourcingdb")]
    eventsourcingdb::KIND,
    #[cfg(feature = "postgres")]
    postgres::KIND,
    #[cfg(feature = "tephra")]
    tephra::KIND,
    #[cfg(feature = "umadb")]
    umadb::KIND,
];

/// What a store's call comes back with, some time later.
type Answer<'a, T, E = String> = Pin<Box<dyn Future<Output = std::result::Result<T, E>> + Send + 'a>>;

/// One store, as its adapter names and reaches it.
struct Kind {
    /// What `/health` calls it, whatever scheme it was reached by.
    name: &'static str,
    /// What the error naming the stores calls it.
    title: &'static str,
    schemes: &'static [&'static str],
    reads_backwards: bool,
    /// Whether the events it reads carry their tags.
    #[cfg_attr(not(test), allow(dead_code))]
    reads_tags: bool,
    connect: fn(Target<'_>) -> Answer<'_, Box<dyn Adapter>, wasmtime::Error>,
}

/// A store URL, split for an adapter to take what it needs of it. Which
/// parts are read depends on the stores built in.
#[allow(dead_code)]
struct Target<'a> {
    scheme: &'a str,
    /// What follows the scheme's `://`.
    address: &'a str,
    url: &'a str,
}

/// What a read asks an adapter for.
enum Query {
    All,
    /// Never none: a query without items is answered before it gets here.
    Items(Vec<QueryItem>),
}

/// A store, as its adapter speaks to it. A condition, when there is
/// one, has items.
trait Adapter: Send + Sync {
    /// The position of the last event; none when the store is empty.
    fn head(&self) -> Answer<'_, Option<u64>>;
    /// Oldest first, unless `options.backwards`.
    fn read(&self, query: Query, options: ReadOptions) -> Answer<'_, Vec<SequencedEvent>>;
    /// Atomically; the position of the last event appended.
    fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Answer<'_, u64, AppendError>;
    fn health(&self) -> Answer<'_, ()>;
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

/// The schemes this build connects to, for the error that names none of them.
fn schemes() -> String {
    let known: Vec<String> = KINDS
        .iter()
        .map(|kind| format!("{}:// for {}", kind.schemes.join(":// or "), kind.title))
        .collect();
    format!("use {}", known.join(", "))
}

pub struct EventStore {
    adapter: Box<dyn Adapter>,
    kind: &'static Kind,
    timeout: Duration,
}

impl EventStore {
    pub async fn connect(url: &str, timeout: Duration) -> Result<Self> {
        let Some((scheme, address)) = url.split_once("://") else {
            return Err(format_err!("not a store URL: {url} – {}", schemes()));
        };
        let Some(kind) = KINDS.iter().find(|kind| kind.schemes.contains(&scheme)) else {
            return Err(format_err!("no store this build connects to at {url} – {}", schemes()));
        };
        let adapter = tokio::time::timeout(timeout, (kind.connect)(Target { scheme, address, url }))
            .await
            .map_err(|_| format_err!("connecting to {url}: no answer within {timeout:?}"))??;
        Ok(Self { adapter, kind, timeout })
    }

    /// The store's name, one per store whatever the scheme it was reached by.
    pub fn kind(&self) -> &'static str {
        self.kind.name
    }

    /// Every `kind` this build knows.
    pub fn kinds() -> Vec<&'static str> {
        KINDS.iter().map(|kind| kind.name).collect()
    }

    async fn bounded<T, E>(&self, what: &str, call: impl Future<Output = std::result::Result<T, E>>, late: impl FnOnce(String) -> E) -> std::result::Result<T, E> {
        match tokio::time::timeout(self.timeout, call).await {
            Ok(result) => result,
            Err(_) => Err(late(format!("{what}: no answer within {:?}", self.timeout))),
        }
    }

    pub async fn head(&self) -> std::result::Result<Option<u64>, String> {
        let started = Instant::now();
        let result = self.bounded("head", self.adapter.head(), |m| m).await;
        match &result {
            Ok(head) => tracing::debug!(target: "store", head, duration_ms = ms(started), "head"),
            Err(reason) => tracing::warn!(target: "store", error = %reason, duration_ms = ms(started), "head failed"),
        }
        result
    }

    /// Why this store cannot read with `options`; none when it can.
    pub fn unsupported(&self, options: &ReadOptions) -> Option<String> {
        (options.backwards && !self.kind.reads_backwards).then(|| format!("{} reads forwards only.", self.kind.title))
    }

    /// The events matching `query` – every event when there is none.
    pub async fn read(&self, query: Option<Vec<QueryItem>>, options: ReadOptions) -> std::result::Result<Vec<SequencedEvent>, String> {
        if let Some(reason) = self.unsupported(&options) {
            return Err(reason);
        }
        let query_json = query.as_deref().map_or(Value::from("everything"), items_json);
        let call = async {
            match query {
                None => self.adapter.read(Query::All, options).await,
                // No items match nothing.
                Some(items) if items.is_empty() => Ok(Vec::new()),
                Some(items) => self.adapter.read(Query::Items(items), options).await,
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
        // No items fail on nothing.
        let condition = condition.filter(|c| !c.fail_if_events_match.is_empty());
        let started = Instant::now();
        let result = self.bounded("append", self.adapter.append(events, condition), AppendError::Other).await;
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
        let started = Instant::now();
        let result = self.bounded("health", self.adapter.health(), |m| m).await;
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
