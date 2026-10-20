//! Axon Server's DCB event store, over gRPC, in a context created as
//! one – the standalone server's with
//! `axoniq.axonserver.standalone-dcb=true`. The URL's path names the
//! context, `default` when it names none; its user is the access token,
//! when access control is on.
//!
//! Axon numbers events from sequence 0; positions here start at 1, as in
//! the other stores, so a position is its sequence + 1. Axon's head – the
//! sequence the next event gets – is then the position of the last.
//!
//! A tag `key:value` is Axon's tag of that key and value, as Axon
//! Framework writes them; a tag without a colon has an empty value. The
//! events Axon sources carry no tags – their tags are a call per event
//! away – so a read returns events without them (`reads_tags`): the
//! component folds types and data only.
//!
//! Axon sources forwards only (`reads_backwards`); a limit stops taking
//! events from the stream, which cancels the rest.

use std::time::{SystemTime, UNIX_EPOCH};

use tonic::metadata::{AsciiMetadataValue, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status};
use wasmtime::format_err;

use super::address::Address;
use super::{Adapter, Answer, AppendCondition, AppendError, Event, Kind, Query, QueryItem, ReadOptions, SequencedEvent, Target};

pub const KIND: Kind = Kind {
    name: "axonserver",
    title: "Axon Server",
    schemes: &["axonserver", "axonserver+tls"],
    reads_backwards: false,
    reads_tags: false,
    connect,
};

fn connect(target: Target<'_>) -> Answer<'_, Box<dyn Adapter>, wasmtime::Error> {
    Box::pin(async move {
        let store = AxonServer::connect(target.address, target.scheme == "axonserver+tls").await?;
        Ok(Box::new(store) as Box<dyn Adapter>)
    })
}

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
struct Headers {
    context: AsciiMetadataValue,
    token: Option<AsciiMetadataValue>,
}

impl Interceptor for Headers {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request.metadata_mut().insert("axoniq-context", self.context.clone());
        if let Some(token) = &self.token {
            request.metadata_mut().insert("axoniq-access-token", token.clone());
        }
        Ok(request)
    }
}

struct AxonServer(DcbEventStoreClient<InterceptedService<Channel, Headers>>);

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
    async fn connect(address: &str, tls: bool) -> wasmtime::Result<Self> {
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

    async fn head(&self) -> Result<Option<u64>, String> {
        let head = self.0.clone().get_head(GetHeadRequest {}).await.map_err(text)?.into_inner().sequence;
        Ok((head > 0).then_some(head as u64))
    }

    /// Backwards reads are refused before they get here.
    async fn read(&self, query: Query, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
        // Axon's empty criteria match all.
        let criterion = match query {
            Query::All => Vec::new(),
            Query::Items(items) => criteria_of(items),
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

    async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
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
        // position after `after`.
        let condition = condition.map(|condition| ConsistencyCondition {
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

    async fn health(&self) -> Result<(), String> {
        self.head().await.map(|_| ())
    }
}

adapter!(AxonServer);

/// A failed consistency condition: Axon cancels the append, and names
/// the exception only in the message – its error code is the generic
/// AXONIQ-0001.
fn is_conflict(status: &Status) -> bool {
    status.code() == Code::Cancelled && status.message().contains("ConsistencyConditionException")
}
