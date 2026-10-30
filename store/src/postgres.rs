//! PostgreSQL backend: one `stepwise_docs` table of `jsonb` documents (same model as SQLite).
//!
//! * Pooled with deadpool; the pool is lazy, and the schema is created on first use (guarded by an
//!   advisory lock so several instances starting together cannot race). `main` pings at startup so
//!   a bad URL fails fast.
//! * Filters and sort fields are *bound parameters* (`doc #>> $n::text[] = $m`), never SQL text.
//!   `#>>` yields `true`/`false`/`2`/`abc`, which is exactly what the SQLite backend compares, so
//!   both backends answer the same query the same way.
//! * RFC 7396 merge patches are applied in Rust inside a `SELECT … FOR UPDATE` transaction.
//! * TLS is used only for `sslmode=require` and then always verifies the server certificate, against
//!   `database.tls_ca_path` or the bundled public roots. Any other mode is plaintext (no silent downgrade).
//! * `database.schema` namespaces everything, so one database can host several Stepwise instances.

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod, Runtime};
use rustls_pki_types::{pem::PemObject, CertificateDer};
use serde_json::Value;
use tokio::sync::OnceCell;
use tokio_postgres::{config::SslMode, types::ToSql, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::{
    effects::{self, Action},
    valid_field, ApplyError, DocStore, Effect, ListQuery, OpResult,
};

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS stepwise_docs (
    collection text        NOT NULL,
    id         text        NOT NULL,
    doc        jsonb       NOT NULL,
    version    bigint      NOT NULL DEFAULT 1,
    seq        bigserial,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (collection, id)
);
ALTER TABLE stepwise_docs ADD COLUMN IF NOT EXISTS version bigint NOT NULL DEFAULT 1;
CREATE INDEX IF NOT EXISTS stepwise_docs_collection_seq ON stepwise_docs (collection, seq);
CREATE TABLE IF NOT EXISTS stepwise_sequences (
    collection text   PRIMARY KEY,
    value      bigint NOT NULL
);";

/// Arbitrary constant: serialises concurrent first-time schema creation across instances.
const SCHEMA_LOCK_KEY: i64 = 727_374_657;

type Param = Box<dyn ToSql + Sync + Send>;

fn refs(p: &[Param]) -> Vec<&(dyn ToSql + Sync)> {
    p.iter()
        .map(|b| b.as_ref() as &(dyn ToSql + Sync))
        .collect()
}

pub fn valid_schema(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn tls_connector(ca_path: Option<&str>) -> Result<MakeRustlsConnect> {
    let mut roots = rustls::RootCertStore::empty();
    match ca_path {
        Some(p) => {
            let mut n = 0;
            for cert in CertificateDer::pem_file_iter(p)
                .with_context(|| format!("cannot read database.tls_ca_path {p}"))?
            {
                roots.add(cert.with_context(|| format!("bad PEM in {p}"))?)?;
                n += 1;
            }
            if n == 0 {
                bail!("no certificates found in {p}");
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("TLS protocol setup")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(MakeRustlsConnect::new(cfg))
}

pub struct PostgresStore {
    pool: Pool,
    schema: String,
    ready: OnceCell<()>,
}

fn be<E: Into<anyhow::Error>>(e: E) -> ApplyError {
    ApplyError::Backend(e.into())
}

fn is_retryable(e: &anyhow::Error) -> bool {
    use tokio_postgres::error::SqlState;
    e.downcast_ref::<tokio_postgres::Error>()
        .and_then(|e| e.code())
        .is_some_and(|c| {
            *c == SqlState::T_R_DEADLOCK_DETECTED || *c == SqlState::T_R_SERIALIZATION_FAILURE
        })
}

impl PostgresStore {
    async fn apply_once(
        &self,
        effects: &[Effect],
    ) -> std::result::Result<Vec<OpResult>, ApplyError> {
        let mut c = self.conn().await.map_err(be)?;
        let tx = c.transaction().await.map_err(be)?;

        // Lock every row the plan touches *in a canonical order* before doing anything else. Two plans
        // that touch the same rows then queue behind each other instead of waiting in a cycle, so
        // opposite-direction transfers (a->b and b->a) cannot deadlock. (Retrying on deadlock stays as
        // a safety net, but each deadlock costs the server's `deadlock_timeout`, so avoid them.)
        let mut keys: Vec<(&str, &str)> = effects
            .iter()
            .map(|e| (e.collection.as_str(), e.id.as_deref().unwrap_or_default()))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        for (collection, id) in keys {
            tx.query_opt(
                "SELECT 1 FROM stepwise_docs WHERE collection = $1 AND id = $2 FOR UPDATE",
                &[&collection, &id],
            )
            .await
            .map_err(be)?;
        }

        let mut out = Vec::with_capacity(effects.len());
        for (i, e) in effects.iter().enumerate() {
            let id = e.id.as_deref().unwrap_or_default();
            let row = tx
                .query_opt("SELECT doc, version FROM stepwise_docs WHERE collection = $1 AND id = $2 FOR UPDATE", &[&e.collection, &id])
                .await
                .map_err(be)?;
            let cur: Option<(Value, u64)> =
                row.map(|r| (r.get::<_, Value>(0), r.get::<_, i64>(1) as u64));
            let action = effects::step(i, e, cur.as_ref())?; // dropping `tx` rolls everything back
            match &action {
                Action::Insert(doc) => {
                    // a concurrent insert of the same id is caught by the primary key
                    let n = tx
                        .execute("INSERT INTO stepwise_docs (collection, id, doc) VALUES ($1, $2, $3) ON CONFLICT (collection, id) DO NOTHING", &[&e.collection, &id, doc])
                        .await
                        .map_err(be)?;
                    if n == 0 {
                        return Err(effects::conflict_error(i, e));
                    }
                }
                Action::Write(doc) => {
                    tx.execute("UPDATE stepwise_docs SET doc = $3, version = version + 1, updated_at = now() WHERE collection = $1 AND id = $2", &[&e.collection, &id, doc])
                        .await
                        .map_err(be)?;
                }
                Action::Remove => {
                    tx.execute(
                        "DELETE FROM stepwise_docs WHERE collection = $1 AND id = $2",
                        &[&e.collection, &id],
                    )
                    .await
                    .map_err(be)?;
                }
                Action::Skip => {}
            }
            out.push(effects::result_for(
                i,
                e,
                &action,
                cur.as_ref().map(|c| c.1),
            ));
        }
        tx.commit().await.map_err(be)?;
        Ok(out)
    }

    /// Does not connect: the pool is lazy. Call `ping()` to verify connectivity.
    pub fn new(
        url: &str,
        schema: &str,
        pool_size: usize,
        tls_ca_path: Option<&str>,
    ) -> Result<Arc<Self>> {
        if !valid_schema(schema) {
            bail!("database.schema `{schema}` must be lower-case letters, digits and underscores, starting with a letter or underscore");
        }
        let mut cfg: tokio_postgres::Config = url
            .parse()
            .context("database.url is not a valid PostgreSQL connection string")?;
        cfg.options(&format!("-c search_path={schema}"));
        cfg.application_name("stepwise");
        if cfg.get_connect_timeout().is_none() {
            cfg.connect_timeout(Duration::from_secs(10));
        }
        let mgr_cfg = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        // TLS is opt-in and never silently downgraded: `sslmode=require` encrypts *and verifies* the
        // server certificate; anything else (including the libpq default, `prefer`) is plaintext.
        let mgr = match cfg.get_ssl_mode() {
            SslMode::Require => Manager::from_config(cfg, tls_connector(tls_ca_path)?, mgr_cfg),
            _ => Manager::from_config(cfg, NoTls, mgr_cfg),
        };
        let pool = Pool::builder(mgr)
            .max_size(pool_size.max(1))
            .runtime(Runtime::Tokio1)
            .wait_timeout(Some(Duration::from_secs(15)))
            .create_timeout(Some(Duration::from_secs(10)))
            .build()
            .context("creating the connection pool")?;
        Ok(Arc::new(Self {
            pool,
            schema: schema.to_string(),
            ready: OnceCell::new(),
        }))
    }

    async fn init_schema(&self) -> Result<()> {
        let mut c = self
            .pool
            .get()
            .await
            .map_err(|e| anyhow!("cannot connect to PostgreSQL: {e}"))?;
        let tx = c.transaction().await?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK_KEY])
            .await?;
        tx.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\";", self.schema))
            .await?;
        tx.batch_execute(SCHEMA_SQL).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn conn(&self) -> Result<Object> {
        self.ready
            .get_or_try_init(|| self.init_schema())
            .await
            .context("initialising the database schema")?;
        self.pool
            .get()
            .await
            .map_err(|e| anyhow!("postgres pool: {e}"))
    }

    /// Delete this instance's schema and everything in it. For tests and decommissioning only.
    pub async fn drop_schema_cascade(&self) -> Result<()> {
        let c = self
            .pool
            .get()
            .await
            .map_err(|e| anyhow!("postgres pool: {e}"))?;
        c.batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            self.schema
        ))
        .await?;
        Ok(())
    }
}

fn path(field: &str) -> Vec<String> {
    field.split('.').map(String::from).collect()
}

/// `collection = $1 AND doc #>> $n::text[] = $m::text …`; field paths and values are parameters.
fn where_clause(
    collection: &str,
    filters: &[(String, String)],
    in_filters: &[(String, Vec<String>)],
    ne_filters: &[(String, String)],
) -> Result<(String, Vec<Param>)> {
    let mut sql = String::from("collection = $1");
    let mut args: Vec<Param> = vec![Box::new(collection.to_string())];
    for (field, value) in filters {
        if !valid_field(field) {
            bail!("invalid filter field `{field}`");
        }
        let p = args.len() + 1;
        sql += &format!(" AND doc #>> ${p}::text[] = ${}::text", p + 1);
        args.push(Box::new(path(field)));
        args.push(Box::new(value.clone()));
    }
    for (field, values) in in_filters {
        if !valid_field(field) {
            bail!("invalid filter field `{field}`");
        }
        let p = args.len() + 1;
        sql += &format!(" AND doc #>> ${p}::text[] = ANY(${}::text[])", p + 1);
        args.push(Box::new(path(field)));
        args.push(Box::new(values.clone()));
    }
    for (field, value) in ne_filters {
        if !valid_field(field) {
            bail!("invalid filter field `{field}`");
        }
        let p = args.len() + 1;
        sql += &format!(
            " AND (doc #>> ${p}::text[]) IS DISTINCT FROM ${}::text",
            p + 1
        );
        args.push(Box::new(path(field)));
        args.push(Box::new(value.clone()));
    }
    Ok((sql, args))
}

#[async_trait]
impl DocStore for PostgresStore {
    async fn get_versioned(&self, collection: &str, id: &str) -> Result<Option<(Value, u64)>> {
        let c = self.conn().await?;
        let row = c
            .query_opt(
                "SELECT doc, version FROM stepwise_docs WHERE collection = $1 AND id = $2",
                &[&collection, &id],
            )
            .await?;
        Ok(row.map(|r| (r.get::<_, Value>(0), r.get::<_, i64>(1) as u64)))
    }

    /// One transaction; every touched row is locked with `SELECT … FOR UPDATE` before it is checked,
    /// so conditions and operators see (and update) the latest committed state. Deadlocks and
    /// serialization failures between concurrent plans are retried transparently.
    async fn apply(&self, plan: &[Effect]) -> std::result::Result<Vec<OpResult>, ApplyError> {
        let effects = effects::prepare(plan)?;
        if effects.is_empty() {
            return Ok(vec![]);
        }
        let mut attempt = 0;
        loop {
            match self.apply_once(&effects).await {
                Err(ApplyError::Backend(e)) if attempt < 4 && is_retryable(&e) => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(10 * attempt)).await;
                }
                other => return other,
            }
        }
    }

    async fn list_rows(&self, collection: &str, q: &ListQuery) -> Result<Vec<(String, Value)>> {
        let (w, mut args) = where_clause(collection, &q.filters, &q.in_filters, &q.ne_filters)?;
        let mut sql = format!("SELECT id, doc FROM stepwise_docs WHERE {w}");
        match &q.sort {
            Some((field, desc)) => {
                if !valid_field(field) {
                    bail!("invalid sort field `{field}`");
                }
                args.push(Box::new(path(field)));
                // NULLS placement matches SQLite (missing values sort first ascending, last descending).
                sql += &format!(
                    " ORDER BY doc #> ${}::text[] {}, seq",
                    args.len(),
                    if *desc {
                        "DESC NULLS LAST"
                    } else {
                        "ASC NULLS FIRST"
                    }
                );
            }
            None => sql += " ORDER BY seq",
        }
        args.push(Box::new(q.limit.min(i64::MAX as usize) as i64));
        sql += &format!(" LIMIT ${}", args.len());
        args.push(Box::new(q.offset.min(i64::MAX as usize) as i64));
        sql += &format!(" OFFSET ${}", args.len());
        let c = self.conn().await?;
        let rows = c.query(&sql, &refs(&args)).await?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, Value>(1)))
            .collect())
    }

    async fn get_many(&self, collection: &str, ids: &[String]) -> Result<Vec<(String, Value)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let c = self.conn().await?;
        let ids: Vec<String> = ids.to_vec();
        let rows = c
            .query(
                "SELECT id, doc FROM stepwise_docs WHERE collection = $1 AND id = ANY($2::text[])",
                &[&collection, &ids],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, Value>(1)))
            .collect())
    }

    async fn count_matching(&self, collection: &str, q: &ListQuery) -> Result<u64> {
        let (w, args) = where_clause(collection, &q.filters, &q.in_filters, &q.ne_filters)?;
        let c = self.conn().await?;
        let row = c
            .query_one(
                &format!("SELECT COUNT(*) FROM stepwise_docs WHERE {w}"),
                &refs(&args),
            )
            .await?;
        Ok(row.get::<_, i64>(0) as u64)
    }

    async fn next_sequence(&self, collection: &str) -> Result<i64> {
        let c = self.conn().await?;
        let row = c
            .query_one(
                "INSERT INTO stepwise_sequences (collection, value) VALUES ($1, 1)
                 ON CONFLICT (collection) DO UPDATE SET value = stepwise_sequences.value + 1 RETURNING value",
                &[&collection],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn bump_sequence(&self, collection: &str, at_least: i64) -> Result<()> {
        let c = self.conn().await?;
        c.execute(
            "INSERT INTO stepwise_sequences (collection, value) VALUES ($1, $2)
             ON CONFLICT (collection) DO UPDATE SET value = GREATEST(stepwise_sequences.value, $2)",
            &[&collection, &at_least],
        )
        .await?;
        Ok(())
    }

    async fn collections(&self) -> Result<Vec<(String, u64)>> {
        let c = self.conn().await?;
        let rows = c.query("SELECT collection, COUNT(*) FROM stepwise_docs GROUP BY collection ORDER BY collection", &[]).await?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as u64))
            .collect())
    }

    async fn clear(&self, collection: &str) -> Result<u64> {
        let mut c = self.conn().await?;
        let tx = c.transaction().await?;
        let n = tx
            .execute(
                "DELETE FROM stepwise_docs WHERE collection = $1",
                &[&collection],
            )
            .await?;
        tx.execute(
            "DELETE FROM stepwise_sequences WHERE collection = $1",
            &[&collection],
        )
        .await?;
        tx.commit().await?;
        Ok(n)
    }

    async fn ping(&self) -> Result<()> {
        let c = self.conn().await?;
        c.query_one("SELECT 1", &[]).await?;
        Ok(())
    }
}
