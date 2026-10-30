//! Declarative writes ("effects") and the pure planner that decides what each one does.
//!
//! Scripts never talk to the database. They *describe* writes in a small JSON language; the store
//! applies the whole plan in **one transaction** together with the route's own operation:
//!
//! ```json
//! { "op": "update", "collection": "products", "id": "sku-1",
//!   "inc": { "stock": -2 }, "require": { "stock": { "gte": 2 } },
//!   "onFail": { "status": 409, "code": "out_of_stock" } }
//! ```
//!
//! Everything semantic lives in [`step`], a pure function over `(effect, current document)`, so
//! every backend behaves identically; backends only lock the row, call `step`, and write the result.

use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::{merge_patch, valid_collection, valid_field};

pub const MAX_EFFECTS_PER_PLAN: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    /// Create; fails with a conflict if the id exists.
    Insert,
    /// Change fields of an existing document with operators (merge/set/inc/push/unset).
    Update,
    /// Overwrite an existing document.
    Replace,
    /// Insert, or overwrite if it exists.
    Upsert,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IfMissing {
    /// Default: a missing document fails the whole plan (404, or `onFail`).
    Fail,
    /// Do nothing for this effect and carry on.
    Skip,
}

/// What the client gets when this effect cannot be applied (precondition failed, document missing,
/// duplicate id).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub status: u16,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Effect {
    pub op: OpKind,
    pub collection: String,
    /// String or integer in JSON; stored as text. Optional only for `insert` (a uuid is generated).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "id_from_json"
    )]
    pub id: Option<String>,
    /// Full document for insert / replace / upsert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<Value>,
    // ---- update operators, applied in this order: merge, set, inc, push, unset
    /// RFC 7396 merge patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<Value>,
    /// `{ "a.b": value }`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<Map<String, Value>>,
    /// `{ "stock": -2 }`; a missing field counts as 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inc: Option<Map<String, Value>>,
    /// `{ "tags": "x" }`; creates the array if missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unset: Option<Vec<String>>,
    /// `replace` / `upsert` only: fields whose value is carried over from the stored document (or stays
    /// absent if it has none), whatever `doc` says. Protects `createdAt`, owners, read-only fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<Vec<String>>,
    // ---- conditions, evaluated against the current document inside the transaction
    /// `{ "stock": { "gte": 2 }, "status": "open" }`; operators: eq ne gt gte lt lte in nin exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<Map<String, Value>>,
    /// Optimistic concurrency: the stored version must equal this (see `If-Match` / `ETag`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fail: Option<Failure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_missing: Option<IfMissing>,
}

fn id_from_json<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Ok(Some(n.to_string())),
        Some(other) => Err(de::Error::custom(format!(
            "id must be a string or an integer, not {other}"
        ))),
    }
}

impl Effect {
    fn base(op: OpKind, collection: &str, id: Option<&str>) -> Self {
        Self {
            op,
            collection: collection.to_string(),
            id: id.map(String::from),
            doc: None,
            merge: None,
            set: None,
            inc: None,
            push: None,
            unset: None,
            keep: None,
            require: None,
            expect_version: None,
            on_fail: None,
            if_missing: None,
        }
    }
    pub fn insert(collection: &str, id: &str, doc: Value) -> Self {
        Self {
            doc: Some(doc),
            ..Self::base(OpKind::Insert, collection, Some(id))
        }
    }
    pub fn replace(collection: &str, id: &str, doc: Value) -> Self {
        Self {
            doc: Some(doc),
            ..Self::base(OpKind::Replace, collection, Some(id))
        }
    }
    pub fn merge_patch(collection: &str, id: &str, patch: Value) -> Self {
        Self {
            merge: Some(patch),
            ..Self::base(OpKind::Update, collection, Some(id))
        }
    }
    pub fn delete(collection: &str, id: &str) -> Self {
        Self::base(OpKind::Delete, collection, Some(id))
    }
    pub fn with_expect_version(mut self, v: Option<u64>) -> Self {
        self.expect_version = v;
        self
    }

    fn has_update_ops(&self) -> bool {
        self.merge.is_some()
            || self.set.is_some()
            || self.inc.is_some()
            || self.push.is_some()
            || self.unset.is_some()
    }

    /// Static checks that need no database. Returns a message fit to show to a script author.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_collection(&self.collection) {
            return Err(format!(
                "collection `{}` must be 1-64 characters of [A-Za-z0-9_-]",
                self.collection
            ));
        }
        if let Some(id) = &self.id {
            if id.is_empty() || id.len() > 256 {
                return Err("id must be 1-256 characters".into());
            }
        }
        let needs_id = !matches!(self.op, OpKind::Insert);
        if needs_id && self.id.is_none() {
            return Err(format!("`{:?}` needs an `id`", self.op).to_lowercase());
        }
        match self.op {
            OpKind::Insert | OpKind::Replace | OpKind::Upsert => {
                if !matches!(self.doc, Some(Value::Object(_))) {
                    return Err(
                        format!("{:?} needs `doc` to be a JSON object", self.op).to_lowercase()
                    );
                }
                if self.has_update_ops() {
                    return Err(
                        "update operators (merge/set/inc/push/unset) only apply to `update`".into(),
                    );
                }
            }
            OpKind::Update => {
                if self.doc.is_some() {
                    return Err(
                        "`update` does not take `doc` (use merge/set/inc/push/unset, or `replace`)"
                            .into(),
                    );
                }
                if !self.has_update_ops() {
                    return Err(
                        "`update` needs at least one of merge, set, inc, push, unset".into(),
                    );
                }
            }
            OpKind::Delete => {
                if self.doc.is_some() || self.has_update_ops() {
                    return Err("`delete` takes no doc or update operators".into());
                }
            }
        }
        if matches!(self.op, OpKind::Insert | OpKind::Upsert)
            && self.expect_version.is_some()
            && self.op == OpKind::Insert
        {
            return Err("`expectVersion` does not apply to `insert`".into());
        }
        let path = |p: &str| -> Result<(), String> {
            if !valid_field(p) {
                return Err(format!("`{p}` is not a valid field path (letters, digits, `_`, `-`, separated by dots)"));
            }
            if p.split('.').next() == Some("id") {
                return Err(
                    "the `id` field is immutable and cannot be changed by update operators".into(),
                );
            }
            Ok(())
        };
        for m in [&self.set, &self.inc, &self.push].into_iter().flatten() {
            for k in m.keys() {
                path(k)?;
            }
        }
        for p in self.unset.iter().flatten() {
            path(p)?;
        }
        if let Some(keep) = &self.keep {
            if !matches!(self.op, OpKind::Replace | OpKind::Upsert) {
                return Err("`keep` only applies to `replace` and `upsert`".into());
            }
            if let Some(p) = keep.iter().find(|p| !valid_field(p)) {
                return Err(format!("keep: `{p}` is not a valid field path"));
            }
        }
        if let Some(inc) = &self.inc {
            if let Some((k, _)) = inc.iter().find(|(_, v)| !v.is_number()) {
                return Err(format!("inc.{k} must be a number"));
            }
        }
        if let Some(m) = &self.merge {
            match m {
                Value::Object(o) if !o.contains_key("id") => {}
                Value::Object(_) => {
                    return Err("merge must not contain `id` (ids are immutable)".into())
                }
                _ => return Err("merge must be a JSON object".into()),
            }
        }
        if let Some(req) = &self.require {
            parse_require(req)?;
        }
        if let Some(f) = &self.on_fail {
            if !(400..=599).contains(&f.status) || f.code.is_empty() || f.code.len() > 64 {
                return Err(
                    "onFail needs a status in 400..=599 and a code of 1-64 characters".into(),
                );
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ conditions

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CondOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    Nin,
    Exists,
}

type Conds<'a> = Vec<(&'a str, Vec<(CondOp, &'a Value)>)>;

fn parse_require(req: &Map<String, Value>) -> Result<Conds<'_>, String> {
    let mut out = Vec::new();
    for (field, spec) in req {
        if !valid_field(field) {
            return Err(format!("require: `{field}` is not a valid field path"));
        }
        let mut ops = Vec::new();
        match spec {
            Value::Object(o) if !o.is_empty() => {
                for (k, v) in o {
                    let op = match k.as_str() {
                        "eq" => CondOp::Eq,
                        "ne" => CondOp::Ne,
                        "gt" => CondOp::Gt,
                        "gte" => CondOp::Gte,
                        "lt" => CondOp::Lt,
                        "lte" => CondOp::Lte,
                        "in" => CondOp::In,
                        "nin" => CondOp::Nin,
                        "exists" => CondOp::Exists,
                        other => return Err(format!("require.{field}: unknown operator `{other}` (eq ne gt gte lt lte in nin exists)")),
                    };
                    if matches!(op, CondOp::In | CondOp::Nin) && !v.is_array() {
                        return Err(format!("require.{field}.{k} needs an array"));
                    }
                    if op == CondOp::Exists && !v.is_boolean() {
                        return Err(format!("require.{field}.exists needs true or false"));
                    }
                    ops.push((op, v));
                }
            }
            other => ops.push((CondOp::Eq, other)), // scalar shorthand: { "status": "open" }
        }
        out.push((field.as_str(), ops));
    }
    Ok(out)
}

pub fn get_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(doc, |v, seg| v.as_object()?.get(seg))
}

fn set_path(doc: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let segs: Vec<&str> = path.split('.').collect();
    let (last, parents) = segs
        .split_last()
        .expect("split yields at least one segment");
    let mut cur = doc;
    for s in parents {
        let obj = cur
            .as_object_mut()
            .ok_or_else(|| format!("cannot set `{path}`: `{s}` is not an object"))?;
        cur = obj
            .entry(s.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    cur.as_object_mut()
        .ok_or_else(|| format!("cannot set `{path}`: parent is not an object"))?
        .insert(last.to_string(), value);
    Ok(())
}

fn remove_path(doc: &mut Value, path: &str) {
    let segs: Vec<&str> = path.split('.').collect();
    let (last, parents) = segs.split_last().expect("at least one segment");
    let mut cur = doc;
    for s in parents {
        match cur.as_object_mut().and_then(|o| o.get_mut(*s)) {
            Some(next) => cur = next,
            None => return,
        }
    }
    if let Some(o) = cur.as_object_mut() {
        o.remove(*last);
    }
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None, // different or non-orderable types never satisfy gt/gte/lt/lte
    }
}

fn holds(op: CondOp, actual: Option<&Value>, want: &Value) -> bool {
    use std::cmp::Ordering::*;
    match op {
        CondOp::Exists => actual.is_some() == want.as_bool().unwrap_or(true),
        CondOp::Eq => actual.is_some_and(|a| json_eq(a, want)),
        CondOp::Ne => !actual.is_some_and(|a| json_eq(a, want)),
        CondOp::In => actual.is_some_and(|a| {
            want.as_array()
                .is_some_and(|l| l.iter().any(|x| json_eq(a, x)))
        }),
        CondOp::Nin => !actual.is_some_and(|a| {
            want.as_array()
                .is_some_and(|l| l.iter().any(|x| json_eq(a, x)))
        }),
        CondOp::Gt => actual.and_then(|a| compare(a, want)) == Some(Greater),
        CondOp::Gte => matches!(actual.and_then(|a| compare(a, want)), Some(Greater | Equal)),
        CondOp::Lt => actual.and_then(|a| compare(a, want)) == Some(Less),
        CondOp::Lte => matches!(actual.and_then(|a| compare(a, want)), Some(Less | Equal)),
    }
}

/// Check the *shape* of a condition map without evaluating it (used to validate `where` clauses).
pub fn validate_require(req: &Map<String, Value>) -> Result<(), String> {
    parse_require(req).map(|_| ())
}

/// `Err(field)` names the first condition that does not hold. A missing document has no fields.
pub fn check_require(doc: Option<&Value>, req: &Map<String, Value>) -> Result<(), String> {
    let conds = parse_require(req)?;
    for (field, ops) in conds {
        let actual = doc.and_then(|d| get_path(d, field));
        if !ops.iter().all(|(op, want)| holds(*op, actual, want)) {
            return Err(field.to_string());
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ update operators

pub fn apply_update(doc: &mut Value, e: &Effect) -> Result<(), String> {
    if let Some(patch) = &e.merge {
        merge_patch(doc, patch);
    }
    for (k, v) in e.set.iter().flatten() {
        set_path(doc, k, v.clone())?;
    }
    for (k, delta) in e.inc.iter().flatten() {
        let new = match get_path(doc, k) {
            None => delta.clone(),
            Some(Value::Number(cur)) => match (cur.as_i64(), delta.as_i64()) {
                (Some(a), Some(b)) => Value::from(
                    a.checked_add(b)
                        .ok_or_else(|| format!("inc.{k} overflows"))?,
                ),
                _ => {
                    let sum = cur.as_f64().unwrap_or_default() + delta.as_f64().unwrap_or_default();
                    serde_json::Number::from_f64(sum)
                        .map(Value::Number)
                        .ok_or_else(|| format!("inc.{k} is not a finite number"))?
                }
            },
            Some(other) => {
                return Err(format!(
                    "cannot inc `{k}`: it is {}, not a number",
                    type_name(other)
                ))
            }
        };
        set_path(doc, k, new)?;
    }
    for (k, item) in e.push.iter().flatten() {
        match get_path(doc, k).cloned() {
            None => set_path(doc, k, Value::Array(vec![item.clone()]))?,
            Some(Value::Array(mut a)) => {
                a.push(item.clone());
                set_path(doc, k, Value::Array(a))?;
            }
            Some(other) => {
                return Err(format!(
                    "cannot push to `{k}`: it is {}, not an array",
                    type_name(&other)
                ))
            }
        }
    }
    for p in e.unset.iter().flatten() {
        remove_path(doc, p);
    }
    Ok(())
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

// ------------------------------------------------------------------ planning

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultStatus {
    Inserted,
    Replaced,
    Updated,
    Deleted,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpResult {
    pub index: usize,
    pub op: OpKind,
    pub collection: String,
    pub id: String,
    pub status: ResultStatus,
    /// New version of the document (absent for deletes and skips).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// The document as stored after the effect (absent for deletes and skips).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<Value>,
}

#[derive(Debug)]
pub enum ApplyError {
    /// A `require` did not hold (or the effect's `onFail` replaced a missing/duplicate error).
    Precondition {
        index: usize,
        field: String,
        on_fail: Option<Failure>,
    },
    NotFound {
        index: usize,
        collection: String,
        id: String,
    },
    Conflict {
        index: usize,
        collection: String,
        id: String,
    },
    VersionMismatch {
        index: usize,
        expected: u64,
        actual: u64,
    },
    /// The effect is malformed or cannot be applied to this document (e.g. `inc` on a string).
    Invalid {
        index: usize,
        message: String,
    },
    Backend(anyhow::Error),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Precondition { index, field, .. } => {
                write!(f, "effect {index}: precondition on `{field}` failed")
            }
            Self::NotFound {
                index,
                collection,
                id,
            } => write!(f, "effect {index}: {collection}/{id} not found"),
            Self::Conflict {
                index,
                collection,
                id,
            } => write!(f, "effect {index}: {collection}/{id} already exists"),
            Self::VersionMismatch {
                index,
                expected,
                actual,
            } => write!(
                f,
                "effect {index}: expected version {expected}, found {actual}"
            ),
            Self::Invalid { index, message } => write!(f, "effect {index}: {message}"),
            Self::Backend(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ApplyError {}

pub fn not_found_error(index: usize, e: &Effect) -> ApplyError {
    match &e.on_fail {
        Some(_) => ApplyError::Precondition {
            index,
            field: "<missing document>".into(),
            on_fail: e.on_fail.clone(),
        },
        None => ApplyError::NotFound {
            index,
            collection: e.collection.clone(),
            id: e.id.clone().unwrap_or_default(),
        },
    }
}

pub fn conflict_error(index: usize, e: &Effect) -> ApplyError {
    match &e.on_fail {
        Some(_) => ApplyError::Precondition {
            index,
            field: "<duplicate id>".into(),
            on_fail: e.on_fail.clone(),
        },
        None => ApplyError::Conflict {
            index,
            collection: e.collection.clone(),
            id: e.id.clone().unwrap_or_default(),
        },
    }
}

/// Validate every effect and fill in ids (a uuid for `insert` without one, also written into the
/// document's `id` field if it has none). Call once before opening a transaction.
pub fn prepare(plan: &[Effect]) -> Result<Vec<Effect>, ApplyError> {
    if plan.len() > MAX_EFFECTS_PER_PLAN {
        return Err(ApplyError::Invalid {
            index: MAX_EFFECTS_PER_PLAN,
            message: format!("a plan may hold at most {MAX_EFFECTS_PER_PLAN} effects"),
        });
    }
    plan.iter()
        .enumerate()
        .map(|(index, e)| {
            e.validate()
                .map_err(|message| ApplyError::Invalid { index, message })?;
            let mut e = e.clone();
            if e.id.is_none() {
                let id = uuid::Uuid::new_v4().to_string();
                if let Some(Value::Object(d)) = e.doc.as_mut() {
                    d.entry("id").or_insert_with(|| Value::String(id.clone()));
                }
                e.id = Some(id);
            }
            Ok(e)
        })
        .collect()
}

/// What the backend must do for one effect.
#[derive(Debug, PartialEq)]
pub enum Action {
    Insert(Value),
    Write(Value),
    Remove,
    Skip,
}

/// The whole semantics of an effect, as a pure function of the current stored state.
/// `current` is `(document, version)` as locked by the backend; `None` if there is no such document.
pub fn step(
    index: usize,
    e: &Effect,
    current: Option<&(Value, u64)>,
) -> Result<Action, ApplyError> {
    let missing = || -> Result<Action, ApplyError> {
        if e.if_missing == Some(IfMissing::Skip) {
            Ok(Action::Skip)
        } else {
            Err(not_found_error(index, e))
        }
    };
    match (e.op, current) {
        (OpKind::Insert, Some(_)) => return Err(conflict_error(index, e)),
        (OpKind::Update | OpKind::Replace | OpKind::Delete, None) => return missing(),
        _ => {}
    }
    if let (Some(expected), Some((_, actual))) = (e.expect_version, current) {
        if expected != *actual {
            return Err(ApplyError::VersionMismatch {
                index,
                expected,
                actual: *actual,
            });
        }
    }
    if let Some(req) = &e.require {
        if let Err(field) = check_require(current.map(|c| &c.0), req) {
            return Err(ApplyError::Precondition {
                index,
                field,
                on_fail: e.on_fail.clone(),
            });
        }
    }
    let doc = || e.doc.clone().unwrap_or(Value::Null);
    Ok(match (e.op, current) {
        (OpKind::Insert, _) | (OpKind::Upsert, None) => Action::Insert(doc()),
        (OpKind::Replace, _) | (OpKind::Upsert, Some(_)) => {
            let mut new = doc();
            if let (Some(keep), Some((cur, _))) = (&e.keep, current) {
                for k in keep {
                    match get_path(cur, k) {
                        Some(v) => set_path(&mut new, k, v.clone())
                            .map_err(|message| ApplyError::Invalid { index, message })?,
                        None => remove_path(&mut new, k),
                    }
                }
            }
            Action::Write(new)
        }
        (OpKind::Update, Some((cur, _))) => {
            let mut new = cur.clone();
            apply_update(&mut new, e).map_err(|message| ApplyError::Invalid { index, message })?;
            Action::Write(new)
        }
        (OpKind::Delete, _) => Action::Remove,
        (OpKind::Update, None) => unreachable!("handled above"),
    })
}

/// Build the public result for an applied action.
pub fn result_for(
    index: usize,
    e: &Effect,
    action: &Action,
    prev_version: Option<u64>,
) -> OpResult {
    let id = e.id.clone().unwrap_or_default();
    let base = |status, version, doc| OpResult {
        index,
        op: e.op,
        collection: e.collection.clone(),
        id: id.clone(),
        status,
        version,
        doc,
    };
    match action {
        Action::Insert(d) => base(ResultStatus::Inserted, Some(1), Some(d.clone())),
        Action::Write(d) => {
            let status = if e.op == OpKind::Update {
                ResultStatus::Updated
            } else {
                ResultStatus::Replaced
            };
            base(status, Some(prev_version.unwrap_or(0) + 1), Some(d.clone()))
        }
        Action::Remove => base(ResultStatus::Deleted, None, None),
        Action::Skip => base(ResultStatus::Skipped, None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fx(v: Value) -> Effect {
        serde_json::from_value(v).expect("effect parses")
    }

    #[test]
    fn effects_parse_from_the_documented_json_shape() {
        let e = fx(
            json!({"op":"update","collection":"products","id":"sku-1","inc":{"stock":-2},
            "require":{"stock":{"gte":2}},"onFail":{"status":409,"code":"out_of_stock"},"expectVersion":3,"ifMissing":"skip"}),
        );
        assert_eq!(
            (e.op, e.expect_version, e.if_missing),
            (OpKind::Update, Some(3), Some(IfMissing::Skip))
        );
        assert_eq!(e.on_fail.unwrap().code, "out_of_stock");
        assert_eq!(
            fx(json!({"op":"delete","collection":"c","id":42}))
                .id
                .as_deref(),
            Some("42"),
            "integer ids are accepted"
        );
        assert!(
            serde_json::from_value::<Effect>(json!({"op":"insert","collection":"c","dco":{}}))
                .is_err(),
            "typos are rejected, not ignored"
        );
        assert!(
            serde_json::from_value::<Effect>(json!({"op":"insert","collection":"c","id":1.5}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<Effect>(json!({"op":"explode","collection":"c"})).is_err()
        );
    }

    #[test]
    fn validation_catches_malformed_effects() {
        let bad = |v: Value| fx(v).validate().unwrap_err();
        assert!(bad(json!({"op":"insert","collection":"c"})).contains("doc"));
        assert!(bad(json!({"op":"insert","collection":"a b","doc":{}})).contains("collection"));
        assert!(bad(json!({"op":"update","collection":"c","id":"1"})).contains("at least one"));
        assert!(
            bad(json!({"op":"update","collection":"c","doc":{},"id":"1","set":{"a":1}}))
                .contains("does not take")
        );
        assert!(
            bad(json!({"op":"update","collection":"c","set":{"a":1}})).contains("needs an `id`")
        );
        assert!(
            bad(json!({"op":"delete","collection":"c","id":"1","set":{"a":1}})).contains("no doc")
        );
        assert!(
            bad(json!({"op":"update","collection":"c","id":"1","set":{"id":2}}))
                .contains("immutable")
        );
        assert!(
            bad(json!({"op":"update","collection":"c","id":"1","merge":{"id":2}}))
                .contains("ids are immutable")
        );
        assert!(
            bad(json!({"op":"update","collection":"c","id":"1","set":{"a b":1}}))
                .contains("field path")
        );
        assert!(
            bad(json!({"op":"update","collection":"c","id":"1","inc":{"n":"x"}}))
                .contains("number")
        );
        assert!(bad(json!({"op":"update","collection":"c","id":"1","set":{"a":1},"require":{"a":{"between":[1,2]}}})).contains("unknown operator"));
        assert!(bad(
            json!({"op":"update","collection":"c","id":"1","set":{"a":1},"require":{"a":{"in":3}}})
        )
        .contains("array"));
        assert!(bad(
            json!({"op":"delete","collection":"c","id":"1","onFail":{"status":200,"code":"x"}})
        )
        .contains("onFail"));
        assert!(
            bad(json!({"op":"insert","collection":"c","doc":{},"expectVersion":1}))
                .contains("insert")
        );
        assert!(
            fx(json!({"op":"upsert","collection":"c","id":"1","doc":{"a":1}}))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn conditions_cover_every_operator_and_type_mismatches() {
        let doc = json!({"stock": 5, "status": "open", "price": 9.5, "tags": null, "owner": {"name": "ann"}});
        let ok = |r: Value| check_require(Some(&doc), r.as_object().unwrap()).is_ok();
        assert!(ok(
            json!({"stock": {"gte": 5, "lte": 5, "gt": 4, "lt": 6, "eq": 5, "ne": 6}})
        ));
        assert!(ok(json!({"status": "open"})), "scalar shorthand means eq");
        assert!(ok(
            json!({"status": {"in": ["open","paid"]}, "price": {"nin": [1, 2]}})
        ));
        assert!(ok(
            json!({"owner.name": "ann", "missing": {"exists": false}, "stock": {"exists": true}})
        ));
        assert!(ok(json!({"stock": 5.0})), "1 and 1.0 are equal");
        assert!(
            ok(json!({"missing": {"ne": 1}})),
            "ne on a missing field holds"
        );
        assert!(!ok(json!({"stock": {"gte": 6}})));
        assert!(
            !ok(json!({"status": {"gt": 3}})),
            "a string is not greater than a number"
        );
        assert!(!ok(json!({"missing": {"eq": null}})), "missing is not null");
        assert!(!ok(json!({"tags": {"eq": 1}})));
        assert_eq!(
            check_require(
                Some(&doc),
                json!({"stock": {"gte": 99}}).as_object().unwrap()
            ),
            Err("stock".into())
        );
        assert!(
            check_require(None, json!({"a": {"exists": false}}).as_object().unwrap()).is_ok(),
            "a missing document has no fields"
        );
        assert!(check_require(None, json!({"a": 1}).as_object().unwrap()).is_err());
    }

    #[test]
    fn update_operators_apply_in_a_fixed_order() {
        let mut d =
            json!({"id":"1","stock":10,"price":1.5,"tags":["a"],"meta":{"x":1,"y":2},"tmp":true});
        let e = fx(json!({"op":"update","collection":"c","id":"1",
            "merge":{"meta":{"y":null,"z":3}}, "set":{"a.b":"deep","meta.x":9}, "inc":{"stock":-4,"price":0.25,"counter":1},
            "push":{"tags":"b","fresh":1}, "unset":["tmp","nope.at.all"]}));
        apply_update(&mut d, &e).unwrap();
        assert_eq!(
            d,
            json!({"id":"1","stock":6,"price":1.75,"tags":["a","b"],"meta":{"x":9,"z":3},"a":{"b":"deep"},"counter":1,"fresh":[1]})
        );
        let mut s = json!({"n":"text","arr":5});
        assert!(apply_update(
            &mut s,
            &fx(json!({"op":"update","collection":"c","id":"1","inc":{"n":1}}))
        )
        .unwrap_err()
        .contains("not a number"));
        assert!(apply_update(
            &mut s,
            &fx(json!({"op":"update","collection":"c","id":"1","push":{"arr":1}}))
        )
        .unwrap_err()
        .contains("not an array"));
        assert!(apply_update(
            &mut s,
            &fx(json!({"op":"update","collection":"c","id":"1","set":{"n.x":1}}))
        )
        .unwrap_err()
        .contains("not an object"));
        let mut big = json!({"n": i64::MAX});
        assert!(apply_update(
            &mut big,
            &fx(json!({"op":"update","collection":"c","id":"1","inc":{"n":1}}))
        )
        .unwrap_err()
        .contains("overflows"));
    }

    #[test]
    fn step_decides_every_op_against_every_state() {
        let cur = (json!({"id":"1","n":1}), 4u64);
        let some = Some(&cur);
        let ins = fx(json!({"op":"insert","collection":"c","id":"1","doc":{"id":"1"}}));
        assert!(matches!(step(0, &ins, None), Ok(Action::Insert(_))));
        assert!(matches!(
            step(0, &ins, some),
            Err(ApplyError::Conflict { .. })
        ));
        let upd = fx(json!({"op":"update","collection":"c","id":"1","inc":{"n":1}}));
        assert!(matches!(
            step(0, &upd, None),
            Err(ApplyError::NotFound { .. })
        ));
        assert_eq!(
            step(0, &upd, some).unwrap(),
            Action::Write(json!({"id":"1","n":2}))
        );
        let skip = fx(json!({"op":"delete","collection":"c","id":"1","ifMissing":"skip"}));
        assert_eq!(step(0, &skip, None).unwrap(), Action::Skip);
        assert_eq!(step(0, &skip, some).unwrap(), Action::Remove);
        let ups = fx(json!({"op":"upsert","collection":"c","id":"1","doc":{"id":"1","n":0}}));
        assert!(
            matches!(step(0, &ups, None), Ok(Action::Insert(_)))
                && matches!(step(0, &ups, some), Ok(Action::Write(_)))
        );
        // onFail turns "missing" and "duplicate" into the caller's chosen error
        let with_fail = |op: Value| {
            let mut v = op;
            v["onFail"] = json!({"status": 409, "code": "mine"});
            fx(v)
        };
        let e = step(
            0,
            &with_fail(json!({"op":"update","collection":"c","id":"1","inc":{"n":1}})),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(e, ApplyError::Precondition { on_fail: Some(ref f), .. } if f.code == "mine")
        );
        let e = step(
            0,
            &with_fail(json!({"op":"insert","collection":"c","id":"1","doc":{}})),
            some,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ApplyError::Precondition {
                on_fail: Some(_),
                ..
            }
        ));
        // versions
        let v =
            fx(json!({"op":"update","collection":"c","id":"1","inc":{"n":1},"expectVersion":3}));
        assert!(matches!(
            step(2, &v, some),
            Err(ApplyError::VersionMismatch {
                index: 2,
                expected: 3,
                actual: 4
            })
        ));
        assert!(step(
            0,
            &fx(json!({"op":"update","collection":"c","id":"1","inc":{"n":1},"expectVersion":4})),
            some
        )
        .is_ok());
        // require is checked against the *current* state
        let r = fx(
            json!({"op":"update","collection":"c","id":"1","inc":{"n":1},"require":{"n":{"gte":2}}}),
        );
        assert!(
            matches!(step(0, &r, some), Err(ApplyError::Precondition { ref field, .. }) if field == "n")
        );
    }

    #[test]
    fn keep_carries_fields_over_from_the_stored_document() {
        let cur = (
            json!({"id":"1","createdAt":"T0","owner":"ann","meta":{"v":1},"name":"old"}),
            3u64,
        );
        let rep = fx(
            json!({"op":"replace","collection":"c","id":"1","keep":["createdAt","owner","meta.v","gone"],
            "doc":{"id":"1","createdAt":"FORGED","owner":"mallory","meta":{"v":99,"w":2},"gone":"sneaky","name":"new"}}),
        );
        assert!(rep.validate().is_ok());
        assert_eq!(step(0, &rep, Some(&cur)).unwrap(), Action::Write(json!({"id":"1","createdAt":"T0","owner":"ann","meta":{"v":1,"w":2},"name":"new"})),
            "kept fields win; a kept field absent from the stored document is removed from the new one");
        assert!(
            fx(json!({"op":"update","collection":"c","id":"1","set":{"a":1},"keep":["a"]}))
                .validate()
                .unwrap_err()
                .contains("keep")
        );
        assert!(
            fx(json!({"op":"replace","collection":"c","id":"1","doc":{},"keep":["a b"]}))
                .validate()
                .is_err()
        );
        let ups = fx(
            json!({"op":"upsert","collection":"c","id":"1","keep":["createdAt"],"doc":{"id":"1","createdAt":"NEW"}}),
        );
        assert_eq!(
            step(0, &ups, None).unwrap(),
            Action::Insert(json!({"id":"1","createdAt":"NEW"})),
            "nothing to keep on a first insert"
        );
    }

    #[test]
    fn prepare_validates_assigns_ids_and_bounds_the_plan() {
        let p = prepare(&[fx(
            json!({"op":"insert","collection":"audit","doc":{"event":"x"}}),
        )])
        .unwrap();
        let id = p[0].id.clone().unwrap();
        assert_eq!(
            p[0].doc.as_ref().unwrap()["id"],
            id.as_str(),
            "generated id is written into the document"
        );
        let keep = prepare(&[fx(
            json!({"op":"insert","collection":"a","doc":{"id":"mine"}}),
        )])
        .unwrap();
        assert_eq!(keep[0].doc.as_ref().unwrap()["id"], "mine");
        let e = prepare(&[
            fx(json!({"op":"insert","collection":"a","doc":{}})),
            fx(json!({"op":"update","collection":"a","id":"1"})),
        ])
        .unwrap_err();
        assert!(
            matches!(e, ApplyError::Invalid { index: 1, .. }),
            "the error names the offending effect"
        );
        let many: Vec<Effect> = (0..=MAX_EFFECTS_PER_PLAN)
            .map(|i| Effect::insert("a", &i.to_string(), json!({})))
            .collect();
        assert!(matches!(prepare(&many), Err(ApplyError::Invalid { .. })));
    }
}
