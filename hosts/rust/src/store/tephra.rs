//! Tephra, over its TCP protocol. It speaks the DCB specification's
//! read and conditional append, so this adapter only translates shapes.

use tephra_client::{
    AppendCondition as TephraCondition, AsyncClient, ClientError, ErrorCode, EventType, Position, Query as TephraQuery,
    QueryItem as TephraItem, Tag, Tags,
};
use wasmtime::format_err;

use super::{Adapter, Answer, AppendCondition, AppendError, Event, Kind, Query, QueryItem, ReadOptions, SequencedEvent, Target};

pub const KIND: Kind = Kind {
    name: "tephra",
    title: "Tephra",
    schemes: &["tephra"],
    reads_backwards: true,
    reads_tags: true,
    connect,
};

fn connect(target: Target<'_>) -> Answer<'_, Box<dyn Adapter>, wasmtime::Error> {
    Box::pin(async move {
        let client = AsyncClient::connect(target.address)
            .await
            .map_err(|e| format_err!("connecting to Tephra at {}: {e}", target.address))?;
        Ok(Box::new(Tephra(client)) as Box<dyn Adapter>)
    })
}

struct Tephra(AsyncClient);

fn query_of(items: Vec<QueryItem>) -> Result<TephraQuery, String> {
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
    Ok(TephraQuery::items(out))
}

impl Tephra {
    /// Positions are dense and start at 1, so the event count is the
    /// tip; an empty store has none.
    async fn head(&self) -> Result<Option<u64>, String> {
        let stats = self.0.stats().await.map_err(|e| e.to_string())?;
        Ok((stats.event_count > 0).then_some(stats.event_count))
    }

    async fn read(&self, query: Query, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
        let query = match query {
            Query::All => TephraQuery::all(),
            Query::Items(items) => query_of(items)?,
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

    async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
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

    async fn health(&self) -> Result<(), String> {
        self.0.stats().await.map(|_| ()).map_err(|e| e.to_string())
    }
}

adapter!(Tephra);
