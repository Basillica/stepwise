//! Storage lives in the `stepwise-store` crate (no Wasmtime dependency, so it builds and tests
//! quickly on its own). This module re-exports it under the old path and adds `open()`, the only
//! piece that needs the server's configuration types.

pub use stepwise_store::*;

use std::sync::Arc;

use anyhow::{bail, Result};

/// Open the configured backend. Neither backend blocks here: SQLite opens its files, Postgres
/// builds a lazy pool (verify connectivity with `ping()`).
pub fn open(cfg: &crate::config::DatabaseConfig) -> Result<Arc<dyn DocStore>> {
    use crate::config::DbType;
    match cfg.db_type {
        DbType::Sqlite => Ok(
            SqliteStore::open(&cfg.path, cfg.pool_size, cfg.busy_timeout_ms)? as Arc<dyn DocStore>,
        ),
        DbType::Postgres => Ok(PostgresStore::new(
            &cfg.url,
            &cfg.schema,
            cfg.pool_size,
            cfg.tls_ca_path.as_deref(),
        )? as Arc<dyn DocStore>),
        other => bail!("database.db_type `{other:?}` is not supported yet"),
    }
}
