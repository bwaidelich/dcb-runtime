//! EventSourcingDB, over its HTTP API. The URL's user is the API token;
//! its path names the subject events are written to, `/` when it names
//! none.
//!
//! EventSourcingDB knows no tags, and wants event types in reverse-domain
//! notation. So an event's type is the URL's `type-prefix` (`dcb.runtime`
//! when it names none) and its own, `dcb.runtime.CourseDefined`; its data
//! is `{"payload": …, "tags": […]}`. Reads and conditions are EventQL
//! queries over the events of that subject and prefix – those this
//! adapter writes; an append condition is such a query, checked on
//! append.
//!
//! Event ids count from 0; positions here start at 1, as in the other
//! stores, so a position is its id + 1.

use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use wasmtime::format_err;

use super::address::Address;
use super::{Adapter, Answer, AppendCondition, AppendError, Event, Kind, Query, QueryItem, ReadOptions, SequencedEvent, Target};

pub const KIND: Kind = Kind {
    name: "eventsourcingdb",
    title: "EventSourcingDB",
    schemes: &["eventsourcingdb", "eventsourcingdb+tls"],
    reads_backwards: true,
    reads_tags: true,
    connect,
};

fn connect(target: Target<'_>) -> Answer<'_, Box<dyn Adapter>, wasmtime::Error> {
    Box::pin(async move {
        let store = EventSourcingDb::connect(target.address, target.scheme == "eventsourcingdb+tls").await?;
        Ok(Box::new(store) as Box<dyn Adapter>)
    })
}

/// The `source` of the events written: the CloudEvents attribute
/// naming who wrote them.
const SOURCE: &str = "urn:dcb-runtime";

struct EventSourcingDb {
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
    async fn connect(address: &str, tls: bool) -> wasmtime::Result<Self> {
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

    /// The query as an EventQL condition on `e`, the events this adapter
    /// writes. An item matches when the event's type is one of its types
    /// (any, when it names none) and its tags hold all the item's.
    fn matching(&self, query: Query) -> String {
        let items = match query {
            Query::All => "TRUE".to_string(),
            Query::Items(items) => items.into_iter().map(|item| self.item(item)).collect::<Vec<_>>().join(" OR "),
        };
        // The tests on the subject and prefix come first: AND stops at
        // the first false, and other events may have no tags to test.
        format!(
            "e.subject == {} AND STARTSWITH(e.type, {}) AND ({items})",
            literal(&self.subject),
            literal(&self.prefix),
        )
    }

    fn item(&self, item: QueryItem) -> String {
        let mut clauses = Vec::new();
        if !item.types.is_empty() {
            let types: Vec<String> =
                item.types.iter().map(|t| format!("e.type == {}", literal(&format!("{}{t}", self.prefix)))).collect();
            clauses.push(format!("({})", types.join(" OR ")));
        }
        clauses.extend(item.tags.iter().map(|tag| format!("e.data.tags CONTAINS {}", literal(tag))));
        if clauses.is_empty() { "TRUE".into() } else { format!("({})", clauses.join(" AND ")) }
    }

    /// The events' order is the store's, as the ids'; the last event of
    /// all subjects is the first read backwards from the root.
    async fn head(&self) -> Result<Option<u64>, String> {
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

    async fn read(&self, query: Query, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
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

    async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
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
        if let Some(condition) = condition {
            // The events after `after` are those from its id on; ids
            // compare as text in a precondition, unless cast.
            let after = condition.after.map(|after| format!("(e.id AS INT) >= {after} AND ")).unwrap_or_default();
            let eventql = format!(
                "FROM e IN events WHERE {after}{} PROJECT INTO COUNT() == 0",
                self.matching(Query::Items(condition.fail_if_events_match))
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

    async fn health(&self) -> Result<(), String> {
        let response = self.post("verify-api-token", json!({})).await?;
        if response.status().is_success() { Ok(()) } else { Err(Self::failure(response).await) }
    }
}

adapter!(EventSourcingDb);

fn id_of(event: &Value) -> Result<u64, String> {
    event["id"].as_str().and_then(|id| id.parse().ok()).ok_or_else(|| format!("an event without id: {event}"))
}
