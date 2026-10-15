//! The event stores the host can connect the component's `store` import
//! to, picked by the store URL's scheme:
//!
//!   umadb://host:port      UmaDB (gRPC)            feature `umadb`
//!   umadb+tls://host:port  UmaDB over TLS
//!   tephra://host:port     Tephra (TCP protocol)   feature `tephra`
//!
//! The scheme names the store, not the transport: two stores may well
//! speak the same one.
//!
//! Each speaks the DCB specification's read and conditional append, so
//! an adapter only translates shapes: the component's query items,
//! events and append condition into the client's, and its errors back –
//! a failed condition as `AppendError::Integrity`, which the component
//! answers by deciding again.
//!
//! Every call is bounded by a timeout. A store that went away does not
//! always say so – a client may wait on a connection nobody will answer
//! – and a request waiting forever is worse than a 503.

#[cfg(not(any(feature = "umadb", feature = "tephra")))]
compile_error!("enable at least one event store: feature `umadb` or `tephra`");

use std::future::Future;
use std::time::Duration;

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
    format!("use {}", known.join(", "))
}

pub struct EventStore {
    backend: Backend,
    timeout: Duration,
}

enum Backend {
    #[cfg(feature = "umadb")]
    Umadb(umadb::Umadb),
    #[cfg(feature = "tephra")]
    Tephra(tephra::Tephra),
}

impl EventStore {
    pub async fn connect(url: &str, timeout: Duration) -> Result<Self> {
        let Some((scheme, address)) = url.split_once("://") else {
            return Err(format_err!("not a store URL: {url} – {}", schemes()));
        };
        let backend = async {
            match scheme {
                #[cfg(feature = "umadb")]
                "umadb" => Ok(Backend::Umadb(umadb::Umadb::connect(&format!("http://{address}")).await?)),
                #[cfg(feature = "umadb")]
                "umadb+tls" => Ok(Backend::Umadb(umadb::Umadb::connect(&format!("https://{address}")).await?)),
                #[cfg(feature = "tephra")]
                "tephra" => Ok(Backend::Tephra(tephra::Tephra::connect(address).await?)),
                _ => Err(format_err!("no store this build connects to at {url} – {}", schemes())),
            }
        };
        let backend = tokio::time::timeout(timeout, backend)
            .await
            .map_err(|_| format_err!("connecting to {url}: no answer within {timeout:?}"))??;
        Ok(Self { backend, timeout })
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
                #[cfg(feature = "umadb")]
                Backend::Umadb(store) => store.head().await,
                #[cfg(feature = "tephra")]
                Backend::Tephra(store) => store.head().await,
            }
        };
        self.bounded("head", call, |m| m).await
    }

    pub async fn read(&self, query: Vec<QueryItem>) -> std::result::Result<Vec<SequencedEvent>, String> {
        let call = async {
            match &self.backend {
                #[cfg(feature = "umadb")]
                Backend::Umadb(store) => store.read(query).await,
                #[cfg(feature = "tephra")]
                Backend::Tephra(store) => store.read(query).await,
            }
        };
        self.bounded("read", call, |m| m).await
    }

    /// A timed-out append may still have landed; the request reports a
    /// failed store, and a retry by the client is decided afresh – it
    /// sees the events if they did.
    pub async fn append(
        &self,
        events: Vec<Event>,
        condition: Option<AppendCondition>,
    ) -> std::result::Result<u64, AppendError> {
        let call = async {
            match &self.backend {
                #[cfg(feature = "umadb")]
                Backend::Umadb(store) => store.append(events, condition).await,
                #[cfg(feature = "tephra")]
                Backend::Tephra(store) => store.append(events, condition).await,
            }
        };
        self.bounded("append", call, AppendError::Other).await
    }

    /// Whether the store answers; the reason when it does not.
    pub async fn health(&self) -> std::result::Result<(), String> {
        let call = async {
            match &self.backend {
                #[cfg(feature = "umadb")]
                Backend::Umadb(store) => store.health().await,
                #[cfg(feature = "tephra")]
                Backend::Tephra(store) => store.health().await,
            }
        };
        self.bounded("health", call, |m| m).await
    }
}

#[cfg(feature = "umadb")]
mod umadb {
    use umadb_client::{AsyncUmaDbClient, ServingStatus};
    use umadb_dcb::{DcbAppendCondition, DcbError, DcbEvent, DcbEventStoreAsync, DcbQuery, DcbQueryItem};
    use wasmtime::{Result, format_err};

    use super::{AppendCondition, AppendError, Event, QueryItem, SequencedEvent};

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

        pub async fn read(&self, query: Vec<QueryItem>) -> Result<Vec<SequencedEvent>, String> {
            let (events, _) = self
                .0
                .read_with_head(Some(query_of(query)), None, false, None)
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

    use super::{AppendCondition, AppendError, Event, QueryItem, SequencedEvent};

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

        pub async fn read(&self, query: Vec<QueryItem>) -> Result<Vec<SequencedEvent>, String> {
            let (events, _) = self
                .0
                .read_all(query_of(query)?, Position::ZERO, None)
                .await
                .map_err(|e| e.to_string())?;
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
