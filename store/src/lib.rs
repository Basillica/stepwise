//! Document storage for Stepwise.
//!
//! * [`DocStore`] is the backend interface; SQLite and PostgreSQL implement it.
//! * **All writes go through [`DocStore::apply`]**: a plan of [`Effect`]s applied in one
//!   transaction with per-document versions, preconditions and atomic operators. The familiar
//!   `insert` / `replace` / `merge` / `delete` are thin wrappers over it, so there is exactly one
//!   set of write semantics (see `effects.rs`).
//! * Reads (`get`, `list`, `count`) are plain queries.

pub mod crud;
pub mod effects;
pub mod postgres;
pub mod query;
pub mod rules;
pub mod sqlite;

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{Map, Value};

pub use effects::{ApplyError, Effect, Failure, IfMissing, OpKind, OpResult, ResultStatus};
pub use postgres::PostgresStore;
pub use sqlite::SqliteStore;

#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Equality filters on (possibly nested, dot-separated) document fields.
    pub filters: Vec<(String, String)>,
    /// `field IN (values…)` filters (same text comparison as `filters`); used for batched relation lookups.
    pub in_filters: Vec<(String, Vec<String>)>,
    /// `field IS NOT value` filters: documents *without* the field match too (null-safe), e.g. "not soft-deleted".
    pub ne_filters: Vec<(String, String)>,
    /// `(field, descending)`
    pub sort: Option<(String, bool)>,
    pub limit: usize,
    pub offset: usize,
}

#[async_trait]
pub trait DocStore: Send + Sync {
    async fn get_versioned(&self, collection: &str, id: &str) -> Result<Option<(Value, u64)>>;
    /// Matching rows as `(row id, document)`, in the requested order (insertion order by default).
    async fn list_rows(&self, collection: &str, q: &ListQuery) -> Result<Vec<(String, Value)>>;
    /// Several documents by row id in one round trip. Missing ids are simply absent; order is unspecified.
    async fn get_many(&self, collection: &str, ids: &[String]) -> Result<Vec<(String, Value)>>;
    /// How many documents match the query's filters (sort/limit/offset are ignored).
    async fn count_matching(&self, collection: &str, q: &ListQuery) -> Result<u64>;
    /// Per-collection auto-increment for integer ids.
    async fn next_sequence(&self, collection: &str) -> Result<i64>;
    /// Make sure future sequence values are greater than `at_least` (after a client-chosen id).
    async fn bump_sequence(&self, collection: &str, at_least: i64) -> Result<()>;
    async fn collections(&self) -> Result<Vec<(String, u64)>>;
    /// Delete every document in a collection (and reset its sequence); returns how many.
    async fn clear(&self, collection: &str) -> Result<u64>;
    async fn ping(&self) -> Result<()>;

    /// Apply every effect in one transaction: all of them take effect, or none do.
    async fn apply(&self, plan: &[Effect]) -> std::result::Result<Vec<OpResult>, ApplyError>;

    // ---- conveniences
    async fn count(&self, collection: &str, filters: &[(String, String)]) -> Result<u64> {
        self.count_matching(
            collection,
            &ListQuery {
                filters: filters.to_vec(),
                ..Default::default()
            },
        )
        .await
    }
    async fn list(&self, collection: &str, q: &ListQuery) -> Result<Vec<Value>> {
        Ok(self
            .list_rows(collection, q)
            .await?
            .into_iter()
            .map(|(_, d)| d)
            .collect())
    }
    async fn get(&self, collection: &str, id: &str) -> Result<Option<Value>> {
        Ok(self.get_versioned(collection, id).await?.map(|(d, _)| d))
    }
    /// `false` if a document with this id already exists.
    async fn insert(&self, collection: &str, id: &str, doc: &Value) -> Result<bool> {
        match self
            .apply(&[Effect::insert(collection, id, doc.clone())])
            .await
        {
            Ok(_) => Ok(true),
            Err(ApplyError::Conflict { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    /// `false` if the document does not exist.
    async fn replace(&self, collection: &str, id: &str, doc: &Value) -> Result<bool> {
        match self
            .apply(&[Effect::replace(collection, id, doc.clone())])
            .await
        {
            Ok(_) => Ok(true),
            Err(ApplyError::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    /// RFC 7396 merge patch; returns the new document, or `None` if it does not exist.
    async fn merge(&self, collection: &str, id: &str, patch: &Value) -> Result<Option<Value>> {
        match self
            .apply(&[Effect::merge_patch(collection, id, patch.clone())])
            .await
        {
            Ok(mut r) => Ok(r.pop().and_then(|x| x.doc)),
            Err(ApplyError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    async fn delete(&self, collection: &str, id: &str) -> Result<bool> {
        match self.apply(&[Effect::delete(collection, id)]).await {
            Ok(_) => Ok(true),
            Err(ApplyError::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn valid_collection(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Dot-separated field path of safe characters; keeps user input out of SQL/JSON-path syntax.
pub fn valid_field(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// RFC 7396 JSON Merge Patch: objects merge recursively, `null` removes a key, anything else replaces.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(p) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            if let Value::Object(t) = target {
                for (k, v) in p {
                    if v.is_null() {
                        t.remove(k);
                    } else {
                        merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
                    }
                }
            }
        }
        other => *target = other.clone(),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Access {
    pub read: bool,
    pub write: bool,
}

impl Access {
    /// `r`, `w` or `rw`.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "r" => Ok(Self {
                read: true,
                write: false,
            }),
            "w" => Ok(Self {
                read: false,
                write: true,
            }),
            "rw" | "wr" => Ok(Self {
                read: true,
                write: true,
            }),
            other => bail!("access `{other}` must be one of: r, w, rw"),
        }
    }
}

/// What a hook may do with the database during one call.
#[derive(Clone)]
pub struct DbAccess {
    pub store: Arc<dyn DocStore>,
    pub grants: Arc<BTreeMap<String, Access>>,
    pub route: Arc<str>,
    pub max_payload: usize,
    /// Named relations (from configuration) that queries may reference.
    pub relations: Arc<query::Relations>,
}

impl DbAccess {
    pub fn check(&self, collection: &str, write: bool) -> std::result::Result<(), String> {
        match self.grants.get(collection) {
            Some(a) if (write && a.write) || (!write && a.read) => Ok(()),
            Some(_) => Err(format!(
                "route `{}` has no {} access to collection `{collection}`",
                self.route,
                if write { "write" } else { "read" }
            )),
            None => Err(format!(
                "collection `{collection}` is not granted to route `{}`",
                self.route
            )),
        }
    }
}

/// Wraps a store and refuses every write. Used for hook test runs so a test can never change real
/// data unless the caller explicitly asks for it.
pub struct ReadOnly(pub Arc<dyn DocStore>);

const READ_ONLY_MSG: &str = "this run is read-only; writes are disabled (pass `db: \"live\"` to the test request to allow them)";

#[async_trait]
impl DocStore for ReadOnly {
    async fn get_versioned(&self, c: &str, id: &str) -> Result<Option<(Value, u64)>> {
        self.0.get_versioned(c, id).await
    }
    async fn list_rows(&self, c: &str, q: &ListQuery) -> Result<Vec<(String, Value)>> {
        self.0.list_rows(c, q).await
    }
    async fn get_many(&self, c: &str, ids: &[String]) -> Result<Vec<(String, Value)>> {
        self.0.get_many(c, ids).await
    }
    async fn count_matching(&self, c: &str, q: &ListQuery) -> Result<u64> {
        self.0.count_matching(c, q).await
    }
    async fn next_sequence(&self, _: &str) -> Result<i64> {
        bail!(READ_ONLY_MSG)
    }
    async fn bump_sequence(&self, _: &str, _: i64) -> Result<()> {
        bail!(READ_ONLY_MSG)
    }
    async fn collections(&self) -> Result<Vec<(String, u64)>> {
        self.0.collections().await
    }
    async fn clear(&self, _: &str) -> Result<u64> {
        bail!(READ_ONLY_MSG)
    }
    async fn ping(&self) -> Result<()> {
        self.0.ping().await
    }
    async fn apply(&self, _: &[Effect]) -> std::result::Result<Vec<OpResult>, ApplyError> {
        Err(ApplyError::Backend(anyhow::anyhow!(READ_ONLY_MSG)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn patched(mut t: Value, p: Value) -> Value {
        merge_patch(&mut t, &p);
        t
    }

    /// The examples from RFC 7396 appendix A.
    #[test]
    fn merge_patch_follows_rfc_7396() {
        assert_eq!(
            patched(json!({"a":"b"}), json!({"a":"c"})),
            json!({"a":"c"})
        );
        assert_eq!(
            patched(json!({"a":"b"}), json!({"b":"c"})),
            json!({"a":"b","b":"c"})
        );
        assert_eq!(patched(json!({"a":"b"}), json!({"a":null})), json!({}));
        assert_eq!(
            patched(json!({"a":"b","b":"c"}), json!({"a":null})),
            json!({"b":"c"})
        );
        assert_eq!(
            patched(json!({"a":["b"]}), json!({"a":"c"})),
            json!({"a":"c"})
        );
        assert_eq!(
            patched(json!({"a":"c"}), json!({"a":["b"]})),
            json!({"a":["b"]})
        );
        assert_eq!(
            patched(json!({"a":{"b":"c"}}), json!({"a":{"b":"d","c":null}})),
            json!({"a":{"b":"d"}})
        );
        assert_eq!(
            patched(json!({"a":[{"b":"c"}]}), json!({"a":[1]})),
            json!({"a":[1]})
        );
        assert_eq!(
            patched(json!(["a", "b"]), json!(["c", "d"])),
            json!(["c", "d"])
        );
        assert_eq!(patched(json!({"a":"b"}), json!(["c"])), json!(["c"]));
        assert_eq!(patched(json!({"a":"foo"}), json!(null)), json!(null));
        assert_eq!(patched(json!({"a":"foo"}), json!("bar")), json!("bar"));
        assert_eq!(
            patched(json!({"e":null}), json!({"a":1})),
            json!({"e":null,"a":1})
        );
        assert_eq!(
            patched(json!([1, 2]), json!({"a":"b","c":null})),
            json!({"a":"b"})
        );
        assert_eq!(
            patched(json!({}), json!({"a":{"bb":{"ccc":null}}})),
            json!({"a":{"bb":{}}})
        );
    }
}
