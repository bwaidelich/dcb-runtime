//! The events in a vector, behind a lock: a store to try a model out
//! with, nothing to start first. Gone when the process ends.
//!
//! It is the reference the other adapters are held to: the conformance
//! suite (`conformance.rs`) runs against it as against them.

use std::sync::Mutex;

use super::{Adapter, Answer, AppendCondition, AppendError, Event, Kind, Query, QueryItem, ReadOptions, SequencedEvent, Target};

pub const KIND: Kind = Kind {
    name: "memory",
    title: "a store in this process, to try a model out",
    schemes: &["memory"],
    reads_backwards: true,
    reads_tags: true,
    connect,
};

fn connect(_: Target<'_>) -> Answer<'_, Box<dyn Adapter>, wasmtime::Error> {
    Box::pin(async { Ok(Box::new(Memory::default()) as Box<dyn Adapter>) })
}

#[derive(Default)]
struct Memory(Mutex<Vec<Event>>);

/// An item matches an event of one of its types (any, when it names
/// none) carrying all its tags.
fn matches(items: &[QueryItem], event: &Event) -> bool {
    items.iter().any(|item| {
        (item.types.is_empty() || item.types.contains(&event.event_type))
            && item.tags.iter().all(|tag| event.tags.contains(tag))
    })
}

impl Memory {
    /// Positions start at 1: the event at index i is at position i + 1.
    fn head_now(&self) -> Result<Option<u64>, String> {
        let events = self.0.lock().map_err(|e| e.to_string())?;
        Ok((!events.is_empty()).then_some(events.len() as u64))
    }

    fn read_now(&self, query: Query, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
        let events = self.0.lock().map_err(|e| e.to_string())?;
        let sequenced = events.iter().enumerate().map(|(i, event)| (i as u64 + 1, event));
        let in_range = |position: u64| match options.after {
            Some(after) if options.backwards => position < after,
            Some(after) => position > after,
            None => true,
        };
        let in_query = |event: &Event| match &query {
            Query::All => true,
            Query::Items(items) => matches(items, event),
        };
        let limit = options.limit.map_or(usize::MAX, |limit| usize::try_from(limit).unwrap_or(usize::MAX));
        let pick = |(position, event): (u64, &Event)| {
            (in_range(position) && in_query(event)).then(|| SequencedEvent { position, event: event.clone() })
        };
        Ok(if options.backwards {
            sequenced.rev().filter_map(pick).take(limit).collect()
        } else {
            sequenced.filter_map(pick).take(limit).collect()
        })
    }

    fn append_now(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
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
}

impl Adapter for Memory {
    fn head(&self) -> Answer<'_, Option<u64>> {
        Box::pin(async { self.head_now() })
    }

    fn read(&self, query: Query, options: ReadOptions) -> Answer<'_, Vec<SequencedEvent>> {
        Box::pin(async move { self.read_now(query, options) })
    }

    fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Answer<'_, u64, AppendError> {
        Box::pin(async move { self.append_now(events, condition) })
    }

    fn health(&self) -> Answer<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}
