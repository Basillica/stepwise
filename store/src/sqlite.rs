//! SQLite backend: one `stepwise_docs` table of JSON documents. WAL mode, a small connection
//! pool, and all blocking work on `spawn_blocking`. Filters/sorting use SQLite's JSON1 with the
//! field path passed as a bound parameter, so request input never becomes SQL text.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rusqlite::{
    params, params_from_iter, types::Value as Sql, Connection, OptionalExtension,
    TransactionBehavior,
};
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::{
    effects::{self, Action},
    valid_field, ApplyError, DocStore, Effect, ListQuery, OpResult,
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS stepwise_docs (
    collection TEXT NOT NULL,
    id         TEXT NOT NULL,
    doc        TEXT NOT NULL CHECK (json_valid(doc)),
    version    INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (collection, id)
);
CREATE TABLE IF NOT EXISTS stepwise_sequences (
    collection TEXT PRIMARY KEY,
    value      INTEGER NOT NULL
);";

struct Pool {
    conns: Mutex<Vec<Connection>>,
    permits: Arc<Semaphore>,
}

pub struct SqliteStore {
    pool: Arc<Pool>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl SqliteStore {
    /// `path` must be a file (an in-memory database would be private to each pooled connection).
    pub fn open(path: &str, pool_size: usize, busy_timeout_ms: u64) -> Result<Arc<Self>> {
        if path == ":memory:" || path.is_empty() {
            bail!("database.path must be a file path");
        }
        if let Some(dir) = Path::new(path)
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
        {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut conns = Vec::new();
        for i in 0..pool_size.max(1) {
            let c = Connection::open(path).with_context(|| format!("opening database {path}"))?;
            c.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms))?;
            // journal_mode returns a row, so read it instead of executing.
            let _: String = c.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
            c.execute_batch("PRAGMA synchronous = NORMAL;")?;
            if i == 0 {
                c.execute_batch(SCHEMA).context("creating schema")?;
                // databases created before versions existed get the column in place
                let has_version = c
                    .prepare("PRAGMA table_info(stepwise_docs)")?
                    .query_map([], |r| r.get::<_, String>(1))?
                    .any(|n| n.as_deref() == Ok("version"));
                if !has_version {
                    c.execute_batch(
                        "ALTER TABLE stepwise_docs ADD COLUMN version INTEGER NOT NULL DEFAULT 1;",
                    )
                    .context("migrating schema (adding version)")?;
                }
            }
            conns.push(c);
        }
        let n = conns.len();
        Ok(Arc::new(Self {
            pool: Arc::new(Pool {
                conns: Mutex::new(conns),
                permits: Arc::new(Semaphore::new(n)),
            }),
        }))
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .pool
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("database pool closed"))?;
        let mut conn = self
            .pool
            .conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .expect("a permit implies a free connection");
        let (conn, res) = tokio::task::spawn_blocking(move || {
            let r = f(&mut conn);
            (conn, r)
        })
        .await
        .map_err(|e| anyhow!("database task failed: {e}"))?;
        self.pool
            .conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(conn);
        drop(permit);
        res.map_err(|e| anyhow!("sqlite: {e}"))
    }
}

/// `WHERE collection = ?1 AND <filter>...` plus its parameters. The field path is bound, never inlined.
fn where_clause(
    collection: &str,
    filters: &[(String, String)],
    in_filters: &[(String, Vec<String>)],
    ne_filters: &[(String, String)],
) -> Result<(String, Vec<Sql>)> {
    let mut sql = String::from("collection = ?1");
    let mut args = vec![Sql::Text(collection.to_string())];
    for (field, value) in filters {
        if !valid_field(field) {
            bail!("invalid filter field `{field}`");
        }
        let p = args.len() + 1;
        // JSON booleans come back from json_extract as 1/0; compare them as 'true'/'false'.
        sql += &format!(
            " AND CASE json_type(doc, ?{p}) WHEN 'true' THEN 'true' WHEN 'false' THEN 'false' \
             ELSE CAST(json_extract(doc, ?{p}) AS TEXT) END = ?{}",
            p + 1
        );
        args.push(Sql::Text(format!("$.{field}")));
        args.push(Sql::Text(value.clone()));
    }
    for (field, values) in in_filters {
        if !valid_field(field) {
            bail!("invalid filter field `{field}`");
        }
        if values.is_empty() {
            sql += " AND 0"; // IN () matches nothing
            continue;
        }
        let p = args.len() + 1;
        args.push(Sql::Text(format!("$.{field}")));
        let marks: Vec<String> = values
            .iter()
            .map(|v| {
                args.push(Sql::Text(v.clone()));
                format!("?{}", args.len())
            })
            .collect();
        sql += &format!(
            " AND CASE json_type(doc, ?{p}) WHEN 'true' THEN 'true' WHEN 'false' THEN 'false' ELSE CAST(json_extract(doc, ?{p}) AS TEXT) END IN ({})",
            marks.join(",")
        );
    }
    for (field, value) in ne_filters {
        if !valid_field(field) {
            bail!("invalid filter field `{field}`");
        }
        let p = args.len() + 1;
        // IS NOT is null-safe: a missing field is "not equal"
        sql += &format!(
            " AND CASE json_type(doc, ?{p}) WHEN 'true' THEN 'true' WHEN 'false' THEN 'false' ELSE CAST(json_extract(doc, ?{p}) AS TEXT) END IS NOT ?{}",
            p + 1
        );
        args.push(Sql::Text(format!("$.{field}")));
        args.push(Sql::Text(value.clone()));
    }
    Ok((sql, args))
}

fn parse(text: String) -> rusqlite::Result<Value> {
    serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

#[async_trait]
impl DocStore for SqliteStore {
    async fn get_versioned(&self, collection: &str, id: &str) -> Result<Option<(Value, u64)>> {
        let (c, id) = (collection.to_string(), id.to_string());
        self.with(move |db| {
            db.query_row(
                "SELECT doc, version FROM stepwise_docs WHERE collection = ?1 AND id = ?2",
                params![c, id],
                |r| Ok((parse(r.get::<_, String>(0)?)?, r.get::<_, i64>(1)? as u64)),
            )
            .optional()
        })
        .await
    }

    /// One `BEGIN IMMEDIATE` transaction: the write lock is taken up front, so every read-check-write
    /// below is serialised against other writers (no lost updates, no oversold stock).
    async fn apply(&self, plan: &[Effect]) -> std::result::Result<Vec<OpResult>, ApplyError> {
        let effects = effects::prepare(plan)?;
        if effects.is_empty() {
            return Ok(vec![]);
        }
        let t = now();
        let res = self
            .with(move |db| {
                let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut out = Vec::with_capacity(effects.len());
                for (i, e) in effects.iter().enumerate() {
                    let id = e.id.as_deref().unwrap_or_default();
                    let cur: Option<(Value, u64)> = tx
                        .query_row("SELECT doc, version FROM stepwise_docs WHERE collection = ?1 AND id = ?2", params![e.collection, id], |r| {
                            Ok((parse(r.get::<_, String>(0)?)?, r.get::<_, i64>(1)? as u64))
                        })
                        .optional()?;
                    let action = match effects::step(i, e, cur.as_ref()) {
                        Ok(a) => a,
                        Err(err) => return Ok(Err(err)), // dropping `tx` rolls everything back
                    };
                    match &action {
                        Action::Insert(doc) => {
                            tx.execute(
                                "INSERT INTO stepwise_docs (collection, id, doc, version, created_at, updated_at) VALUES (?1, ?2, ?3, 1, ?4, ?4)",
                                params![e.collection, id, doc.to_string(), t],
                            )?;
                        }
                        Action::Write(doc) => {
                            tx.execute(
                                "UPDATE stepwise_docs SET doc = ?3, version = version + 1, updated_at = ?4 WHERE collection = ?1 AND id = ?2",
                                params![e.collection, id, doc.to_string(), t],
                            )?;
                        }
                        Action::Remove => {
                            tx.execute("DELETE FROM stepwise_docs WHERE collection = ?1 AND id = ?2", params![e.collection, id])?;
                        }
                        Action::Skip => {}
                    }
                    out.push(effects::result_for(i, e, &action, cur.as_ref().map(|c| c.1)));
                }
                tx.commit()?;
                Ok(Ok(out))
            })
            .await
            .map_err(ApplyError::Backend)?;
        res
    }

    async fn list_rows(&self, collection: &str, q: &ListQuery) -> Result<Vec<(String, Value)>> {
        let (mut sql, mut args) =
            where_clause(collection, &q.filters, &q.in_filters, &q.ne_filters)?;
        sql = format!("SELECT id, doc FROM stepwise_docs WHERE {sql}");
        match &q.sort {
            Some((field, desc)) => {
                if !valid_field(field) {
                    bail!("invalid sort field `{field}`");
                }
                args.push(Sql::Text(format!("$.{field}")));
                sql += &format!(
                    " ORDER BY json_extract(doc, ?{}) {}, rowid",
                    args.len(),
                    if *desc { "DESC" } else { "ASC" }
                );
            }
            None => sql += " ORDER BY rowid",
        }
        args.push(Sql::Integer(q.limit.min(i64::MAX as usize) as i64));
        sql += &format!(" LIMIT ?{}", args.len());
        args.push(Sql::Integer(q.offset.min(i64::MAX as usize) as i64));
        sql += &format!(" OFFSET ?{}", args.len());
        self.with(move |db| {
            let mut st = db.prepare_cached(&sql)?;
            let rows = st.query_map(params_from_iter(args), |r| {
                Ok((r.get::<_, String>(0)?, parse(r.get::<_, String>(1)?)?))
            })?;
            rows.collect()
        })
        .await
    }

    async fn get_many(&self, collection: &str, ids: &[String]) -> Result<Vec<(String, Value)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut args = vec![Sql::Text(collection.to_string())];
        let marks: Vec<String> = ids
            .iter()
            .map(|id| {
                args.push(Sql::Text(id.clone()));
                format!("?{}", args.len())
            })
            .collect();
        let sql = format!(
            "SELECT id, doc FROM stepwise_docs WHERE collection = ?1 AND id IN ({})",
            marks.join(",")
        );
        self.with(move |db| {
            let mut st = db.prepare(&sql)?;
            let rows = st.query_map(params_from_iter(args), |r| {
                Ok((r.get::<_, String>(0)?, parse(r.get::<_, String>(1)?)?))
            })?;
            rows.collect()
        })
        .await
    }

    async fn count_matching(&self, collection: &str, q: &ListQuery) -> Result<u64> {
        let (sql, args) = where_clause(collection, &q.filters, &q.in_filters, &q.ne_filters)?;
        self.with(move |db| {
            let n: i64 = db.query_row(
                &format!("SELECT COUNT(*) FROM stepwise_docs WHERE {sql}"),
                params_from_iter(args),
                |r| r.get(0),
            )?;
            Ok(n as u64)
        })
        .await
    }

    async fn next_sequence(&self, collection: &str) -> Result<i64> {
        let c = collection.to_string();
        self.with(move |db| {
            db.query_row(
                "INSERT INTO stepwise_sequences (collection, value) VALUES (?1, 1)
                 ON CONFLICT (collection) DO UPDATE SET value = value + 1 RETURNING value",
                params![c],
                |r| r.get(0),
            )
        })
        .await
    }

    async fn bump_sequence(&self, collection: &str, at_least: i64) -> Result<()> {
        let c = collection.to_string();
        self.with(move |db| {
            db.execute(
                "INSERT INTO stepwise_sequences (collection, value) VALUES (?1, ?2)
                 ON CONFLICT (collection) DO UPDATE SET value = MAX(value, ?2)",
                params![c, at_least],
            )?;
            Ok(())
        })
        .await
    }

    async fn collections(&self) -> Result<Vec<(String, u64)>> {
        self.with(|db| {
            let mut st = db.prepare("SELECT collection, COUNT(*) FROM stepwise_docs GROUP BY collection ORDER BY collection")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
            rows.collect()
        })
        .await
    }

    async fn clear(&self, collection: &str) -> Result<u64> {
        let c = collection.to_string();
        self.with(move |db| {
            let tx = db.transaction()?;
            let n = tx.execute(
                "DELETE FROM stepwise_docs WHERE collection = ?1",
                params![c],
            )?;
            tx.execute(
                "DELETE FROM stepwise_sequences WHERE collection = ?1",
                params![c],
            )?;
            tx.commit()?;
            Ok(n as u64)
        })
        .await
    }

    async fn ping(&self) -> Result<()> {
        self.with(|db| {
            db.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                .map(|_| ())
        })
        .await
    }
}
