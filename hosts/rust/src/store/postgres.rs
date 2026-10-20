//! PostgreSQL, which knows nothing of DCB: this adapter keeps the events
//! in a table of its own and checks the append condition itself.
//!
//! Events live in one table, `dcb_events`, created when missing; a
//! `search_path` in the URL puts it in a schema of its own. Positions
//! come from an identity column: a failed append may leave a gap, which
//! DCB allows.
//!
//! An append takes a transaction-scoped advisory lock before it checks
//! the condition, so appends happen one at a time: no event can land
//! between the check and the insert, and none commits ahead of an
//! earlier position a reader could still be waiting for.

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::config::SslMode;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Config, NoTls};
use wasmtime::format_err;

use super::{Adapter, Answer, AppendCondition, AppendError, Event, Kind, Query, QueryItem, ReadOptions, SequencedEvent, Target};

pub const KIND: Kind = Kind {
    name: "postgres",
    title: "PostgreSQL",
    schemes: &["postgres", "postgresql"],
    reads_backwards: true,
    reads_tags: true,
    connect,
};

/// The whole URL: it is PostgreSQL's own.
fn connect(target: Target<'_>) -> Answer<'_, Box<dyn Adapter>, wasmtime::Error> {
    Box::pin(async move { Ok(Box::new(Postgres::connect(target.url).await?) as Box<dyn Adapter>) })
}

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

struct Postgres(Pool);

type Params = Vec<Box<dyn ToSql + Sync + Send>>;

/// The items as a condition on a row, their values appended to
/// `params`. An item matches when the event's type is one of its types
/// (any, when it names none) and the event carries all its tags.
fn matching(items: Vec<QueryItem>, params: &mut Params) -> String {
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
    /// Connects once, so a wrong URL or an absent server fails now, and
    /// creates the table when it is missing – under the append lock, as
    /// two hosts starting at once would race to.
    async fn connect(url: &str) -> wasmtime::Result<Self> {
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

    async fn head(&self) -> Result<Option<u64>, String> {
        let client = self.0.get().await.map_err(|e| e.to_string())?;
        let row = client.query_one("SELECT max(position) FROM dcb_events", &[]).await.map_err(|e| e.to_string())?;
        Ok(row.get::<_, Option<i64>>(0).map(|p| p as u64))
    }

    async fn read(&self, query: Query, options: ReadOptions) -> Result<Vec<SequencedEvent>, String> {
        let mut params = Params::new();
        let mut condition = match query {
            Query::All => "TRUE".to_string(),
            Query::Items(items) => format!("({})", matching(items, &mut params)),
        };
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

    async fn append(&self, events: Vec<Event>, condition: Option<AppendCondition>) -> Result<u64, AppendError> {
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

    async fn health(&self) -> Result<(), String> {
        let client = self.0.get().await.map_err(|e| e.to_string())?;
        client.query_one("SELECT 1", &[]).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

adapter!(Postgres);
