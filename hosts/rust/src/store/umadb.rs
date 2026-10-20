//! UmaDB, over gRPC. It speaks the DCB specification's read and
//! conditional append, so this adapter only translates shapes.

use umadb_client::{AsyncUmaDbClient, ServingStatus};
use umadb_dcb::{DcbAppendCondition, DcbError, DcbEvent, DcbEventStoreAsync, DcbQuery, DcbQueryItem};
use wasmtime::format_err;

use super::{Adapter, Answer, AppendCondition, AppendError, Event, Kind, Query, QueryItem, ReadOptions, SequencedEvent, Target};

pub const KIND: Kind = Kind {
    name: "umadb",
    title: "UmaDB",
    schemes: &["umadb", "umadb+tls"],
    reads_backwards: true,
    reads_tags: true,
    connect,
};

fn connect(target: Target<'_>) -> Answer<'_, Box<dyn Adapter>, wasmtime::Error> {
    Box::pin(async move {
        let url = format!("{}://{}", if target.scheme == "umadb+tls" { "https" } else { "http" }, target.address);
        let client = AsyncUmaDbClient::connect(url.clone(), None, None, None)
            .await
            .map_err(|e| format_err!("connecting to UmaDB at {url}: {e}"))?;
        Ok(Box::new(Umadb(client)) as Box<dyn Adapter>)
    })
}

struct Umadb(AsyncUmaDbClient);

fn query_of(items: Vec<QueryItem>) -> DcbQuery {
    items.into_iter().fold(DcbQuery::new(), |query, item| {
        query.item(DcbQueryItem::new().types(item.types).tags(item.tags))
    })
}

impl Umadb {
    async fn head(&self) -> Result<Option<u64>, String> {
        self.0.head().await.map_err(|e| e.to_string())
    }

    async fn read(&self, query: Query, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
        let query = match query {
            Query::All => None,
            Query::Items(items) => Some(query_of(items)),
        };
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
            .read_with_head(query, start, options.backwards, limit)
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

    async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
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

    async fn health(&self) -> Result<(), String> {
        match self.0.check_health("").await {
            Ok(ServingStatus::Serving) => Ok(()),
            Ok(status) => Err(format!("not serving: {status:?}")),
            Err(e) => Err(e.to_string()),
        }
    }
}

adapter!(Umadb);
