//! What every store must do, checked through `EventStore` – the
//! interface the rest of the host has – so the adapter and what is said
//! here once for all of them are checked together.
//!
//! The in-memory store is checked always. Every other store is checked
//! against a server named by an environment variable, and skipped when
//! there is none:
//!
//!   DCB_TEST_AXONSERVER=axonserver://localhost:8124
//!   DCB_TEST_EVENTSOURCINGDB=eventsourcingdb://secret@localhost:3000
//!   DCB_TEST_POSTGRES=postgres://postgres:secret@localhost:5432/postgres
//!   DCB_TEST_TEPHRA=tephra://localhost:9000
//!   DCB_TEST_UMADB=umadb://localhost:50051
//!
//! The server may hold events already: each run writes types and tags
//! of its own and reads only those, from the head it found on. Nothing
//! else may write while it runs.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{AppendCondition, AppendError, Event, EventStore, QueryItem, ReadOptions, SequencedEvent};

/// How long a server just started may take to answer.
const STARTING: Duration = Duration::from_secs(120);

/// A store, retried while it is still starting.
async fn connect(url: &str) -> EventStore {
    let started = Instant::now();
    loop {
        match EventStore::connect(url, Duration::from_secs(10)).await {
            Ok(store) => return store,
            Err(e) if started.elapsed() > STARTING => panic!("connecting to {url}: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// A word no earlier run wrote.
fn fresh() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{:x}{:x}", nanos, std::process::id())
}

fn item(types: &[&str], tags: &[&str]) -> QueryItem {
    QueryItem {
        types: types.iter().map(|t| t.to_string()).collect(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
    }
}

fn event(event_type: &str, tags: &[&str], n: u32) -> Event {
    Event {
        event_type: event_type.to_string(),
        data: format!(r#"{{"n":{n}}}"#),
        tags: tags.iter().map(|t| t.to_string()).collect(),
    }
}

fn condition(items: Vec<QueryItem>, after: Option<u64>) -> Option<AppendCondition> {
    Some(AppendCondition { fail_if_events_match: items, after })
}

/// What a read returned, by the `n` in each event's data.
fn ns(events: &[SequencedEvent]) -> Vec<u64> {
    events
        .iter()
        .map(|e| serde_json::from_str::<Value>(&e.event.data).unwrap()["n"].as_u64().unwrap())
        .collect()
}

async fn suite(url: &str) {
    let store = connect(url).await;
    let kind = store.kind;
    store.health().await.expect("health");
    let start = store.head().await.expect("head");

    let run = fresh();
    let (a, b, c) = (format!("Alpha{run}"), format!("Beta{run}"), format!("Gamma{run}"));
    let (one, two) = (format!("Run{run}:one"), format!("Run{run}:two"));
    let (a, b, c, one, two) = (a.as_str(), b.as_str(), c.as_str(), one.as_str(), two.as_str());
    let ours = || Some(vec![item(&[a], &[]), item(&[b], &[]), item(&[c], &[])]);
    let from_start = ReadOptions { after: start, ..ReadOptions::default() };
    let read = async |query: Option<Vec<QueryItem>>, options: ReadOptions| store.read(query, options).await.expect("read");

    // 1: a [one]; 2: b [one, two]; 3: a [two] – the last two in one append.
    let first = store.append(vec![event(a, &[one], 1)], None).await.expect("append");
    assert!(first >= 1 && first > start.unwrap_or(0), "positions start at 1 and grow: {first} after {start:?}");
    let last = store.append(vec![event(b, &[one, two], 2), event(a, &[two], 3)], None).await.expect("append");
    assert!(last > first, "the position of the last event appended: {last} after {first}");
    assert!(store.head().await.expect("head") >= Some(last), "head is the last position");

    let all = read(ours(), from_start).await;
    assert_eq!(ns(&all), [1, 2, 3], "oldest first");
    assert_eq!(all[0].position, first, "the first event is where its append said");
    assert_eq!(all[2].position, last, "the last event is where its append said");
    assert!(all.windows(2).all(|w| w[0].position < w[1].position), "positions grow");
    assert_eq!(all[1].event.event_type, b, "types round-trip");
    if kind.reads_tags {
        let mut tags = all[1].event.tags.clone();
        tags.sort();
        assert_eq!(tags, [one, two], "tags round-trip");
    }
    let (p1, p2, p3) = (all[0].position, all[1].position, all[2].position);

    assert_eq!(ns(&read(None, from_start).await), [1, 2, 3], "no query is every event");
    assert_eq!(ns(&read(Some(vec![]), from_start).await), [] as [u64; 0], "no items match nothing");
    assert_eq!(ns(&read(Some(vec![item(&[a], &[])]), from_start).await), [1, 3], "by type");
    assert_eq!(ns(&read(Some(vec![item(&[a, b], &[])]), from_start).await), [1, 2, 3], "any of the types");
    assert_eq!(ns(&read(Some(vec![item(&[], &[one])]), from_start).await), [1, 2], "by tag");
    assert_eq!(ns(&read(Some(vec![item(&[], &[one, two])]), from_start).await), [2], "every one of the tags");
    assert_eq!(ns(&read(Some(vec![item(&[a], &[two])]), from_start).await), [3], "by type and tag");
    assert_eq!(
        ns(&read(Some(vec![item(&[b], &[]), item(&[a], &[two])]), from_start).await),
        [2, 3],
        "items are ORed"
    );

    let forwards = |after: Option<u64>, limit: Option<u64>| ReadOptions { after, limit, backwards: false };
    assert_eq!(ns(&read(ours(), forwards(Some(p1), None)).await), [2, 3], "after is exclusive");
    assert_eq!(ns(&read(ours(), forwards(Some(p3), None)).await), [] as [u64; 0], "nothing after the last");
    assert_eq!(ns(&read(ours(), forwards(start, Some(2))).await), [1, 2], "limit");

    let backwards = |after: Option<u64>, limit: Option<u64>| ReadOptions { after, limit, backwards: true };
    if kind.reads_backwards {
        assert_eq!(ns(&read(ours(), backwards(None, None)).await), [3, 2, 1], "backwards, newest first");
        assert_eq!(ns(&read(ours(), backwards(Some(p3), None)).await), [2, 1], "backwards, before is exclusive");
        assert_eq!(ns(&read(ours(), backwards(None, Some(1))).await), [3], "backwards, limit");
        assert_eq!(ns(&read(ours(), backwards(Some(p1), None)).await), [] as [u64; 0], "nothing before the first");
    } else {
        assert!(store.unsupported(&backwards(None, None)).is_some(), "says it cannot read backwards");
        assert!(store.read(ours(), backwards(None, None)).await.is_err(), "refuses to read backwards");
    }

    let appended = |result: Result<u64, AppendError>, what: &str| match result {
        Ok(position) => position,
        Err(AppendError::Integrity) => panic!("{what}: the condition failed"),
        Err(AppendError::Other(e)) => panic!("{what}: {e}"),
    };
    let refused = |result: Result<u64, AppendError>, what: &str| match result {
        Err(AppendError::Integrity) => {}
        Ok(position) => panic!("{what}: appended at {position}"),
        Err(AppendError::Other(e)) => panic!("{what}: {e}"),
    };
    let gamma = |n| vec![event(c, &[], n)];
    refused(store.append(gamma(9), condition(vec![item(&[a], &[])], None)).await, "a match, no after: the whole log");
    refused(store.append(gamma(9), condition(vec![item(&[a], &[])], Some(p1))).await, "a match after `after`");
    refused(store.append(gamma(9), condition(vec![item(&[b], &[]), item(&[], &[two])], Some(p2))).await, "items are ORed");
    let p4 = appended(store.append(gamma(4), condition(vec![item(&[a], &[])], Some(p3))).await, "no match after `after`");
    let p5 = appended(store.append(gamma(5), condition(vec![item(&[], &[one])], Some(p2))).await, "a match only at `after`");
    let p6 = appended(store.append(gamma(6), condition(vec![item(&[format!("Delta{run}").as_str()], &[])], None)).await, "no match");
    let p7 = appended(store.append(gamma(7), condition(vec![], None)).await, "no items fail on nothing");
    assert!(p3 < p4 && p4 < p5 && p5 < p6 && p6 < p7, "positions grow: {p3} {p4} {p5} {p6} {p7}");
    assert_eq!(ns(&read(ours(), from_start).await), [1, 2, 3, 4, 5, 6, 7], "refused appends left nothing");
    assert_eq!(store.head().await.expect("head"), Some(p7), "head is the last position");
}

#[tokio::test]
async fn memory() {
    suite("memory://").await;
}

/// The suite against the store at `$var`, when there is one.
macro_rules! against {
    ($store:ident, $feature:literal, $var:literal) => {
        #[cfg(feature = $feature)]
        #[tokio::test]
        async fn $store() {
            match std::env::var($var) {
                Ok(url) => suite(&url).await,
                Err(_) => eprintln!("skipped: {} names no store", $var),
            }
        }
    };
}

against!(axonserver, "axonserver", "DCB_TEST_AXONSERVER");
against!(eventsourcingdb, "eventsourcingdb", "DCB_TEST_EVENTSOURCINGDB");
against!(postgres, "postgres", "DCB_TEST_POSTGRES");
against!(tephra, "tephra", "DCB_TEST_TEPHRA");
against!(umadb, "umadb", "DCB_TEST_UMADB");
