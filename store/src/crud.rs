//! Spec-driven CRUD: work out from an OpenAPI operation what it should do to the database
//! (`GET /pets` → list, `POST /pets` → create, `GET /pets/{id}` → read, ...) and execute it.
//!
//! Inference is deliberately conservative. Anything that does not look like a resource
//! (`GET /health`, `POST /pets/{id}/adopt`) gets no CRUD and keeps its mock response, unless the
//! route's `crud:` config says otherwise.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{
    effects::{ApplyError, Effect, Failure, OpResult},
    rules::{self, DataRules},
    valid_collection, valid_field, DocStore, ListQuery,
};

/// Per-route overrides of the inferred behaviour. Everything is optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CrudConfig {
    pub collection: Option<String>,
    /// create | read | list | replace | merge | delete | none (`none` keeps the mock response)
    pub operation: Option<Op>,
    pub id_param: Option<String>,
    /// Document field holding the id (default `id`).
    pub id_field: Option<String>,
    /// integer (auto-increment) | string (uuid). Default: taken from the schema.
    pub id_kind: Option<IdKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Create,
    Read,
    List,
    Replace,
    Merge,
    Delete,
    /// Opt out: keep the mock response for this operation.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdKind {
    Integer,
    String,
}

#[derive(Debug, Clone)]
pub struct CrudPlan {
    pub op: Op,
    pub collection: String,
    pub id_param: Option<String>,
    pub id_field: String,
    pub id_kind: IdKind,
}

/// What `infer` looks at.
pub struct Shape<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub has_body: bool,
    /// Schema of the chosen 2xx JSON response (refs already resolved).
    pub response_schema: Option<&'a Value>,
    /// Declared JSON-schema `type` of the last path parameter, if any.
    pub id_param_type: Option<&'a str>,
}

fn is_array(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(t)) => t == "array",
        Some(Value::Array(a)) => a.iter().any(|t| t == "array"),
        _ => schema.get("items").is_some(),
    }
}

/// JSON-schema `type` of `field`, looking through `allOf` / first `oneOf`/`anyOf` branch.
fn property_type(schema: &Value, field: &str) -> Option<String> {
    if let Some(p) = schema.get("properties").and_then(|p| p.get(field)) {
        return match p.get("type") {
            Some(Value::String(t)) => Some(t.clone()),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .find(|t| *t != "null")
                .map(String::from),
            _ => None,
        };
    }
    for key in ["allOf", "oneOf", "anyOf"] {
        if let Some(Value::Array(subs)) = schema.get(key) {
            if let Some(t) = subs.iter().find_map(|s| property_type(s, field)) {
                return Some(t);
            }
        }
    }
    None
}

pub fn infer(shape: &Shape, over: &CrudConfig) -> Result<Option<CrudPlan>> {
    let segs: Vec<&str> = shape.path.split('/').filter(|s| !s.is_empty()).collect();
    let last_param = segs
        .last()
        .and_then(|s| s.strip_prefix('{'))
        .and_then(|s| s.strip_suffix('}'));
    let statics: Vec<&str> = segs
        .iter()
        .copied()
        .filter(|s| !s.starts_with('{'))
        .collect();

    let inferred = match (shape.method, last_param.is_some()) {
        ("GET", false) if shape.response_schema.is_some_and(is_array) => Op::List,
        ("GET", true) => Op::Read,
        ("POST", false) if shape.has_body => Op::Create,
        ("PUT", true) if shape.has_body => Op::Replace,
        ("PATCH", true) if shape.has_body => Op::Merge,
        ("DELETE", true) => Op::Delete,
        _ => Op::None,
    };
    let op = over.operation.unwrap_or(inferred);
    if op == Op::None {
        return Ok(None);
    }

    let collection = match (&over.collection, statics.last()) {
        (Some(c), _) => c.clone(),
        (None, Some(s)) => s.to_string(),
        (None, None) => bail!(
            "cannot derive a collection name from `{}`; set `crud.collection`",
            shape.path
        ),
    };
    if !valid_collection(&collection) {
        bail!("collection `{collection}` must be 1-64 characters of [A-Za-z0-9_-]");
    }
    let id_param = over
        .id_param
        .clone()
        .or_else(|| last_param.map(String::from));
    if matches!(op, Op::Read | Op::Replace | Op::Merge | Op::Delete) && id_param.is_none() {
        bail!("{op:?} on `{}` needs an id path parameter; set `crud.id_param` or use a path ending in {{id}}", shape.path);
    }
    let id_field = over.id_field.clone().unwrap_or_else(|| "id".into());
    if !valid_field(&id_field) || id_field.contains('.') {
        bail!("crud.id_field `{id_field}` must be a plain field name");
    }
    let item = match (op, shape.response_schema) {
        (Op::List, Some(s)) => s.get("items"),
        (_, s) => s,
    };
    let id_kind = over.id_kind.unwrap_or_else(|| {
        match item
            .and_then(|s| property_type(s, &id_field))
            .as_deref()
            .or(shape.id_param_type)
        {
            Some("integer") => IdKind::Integer,
            _ => IdKind::String,
        }
    });
    Ok(Some(CrudPlan {
        op,
        collection,
        id_param,
        id_field,
        id_kind,
    }))
}

// ------------------------------------------------------------------ execution

/// An error already shaped for HTTP, but free of any web framework.
#[derive(Debug, Clone)]
pub struct CrudError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
    pub headers: Vec<(String, String)>,
}

impl CrudError {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: None,
            headers: vec![],
        }
    }
}

/// How a failed plan is presented: the effect's own `onFail` if it has one, else a sensible default.
pub fn http_error(e: &ApplyError) -> CrudError {
    match e {
        ApplyError::Precondition {
            on_fail: Some(f),
            field,
            index,
        } => {
            let mut err = CrudError::new(
                f.status,
                &f.code,
                f.message
                    .clone()
                    .unwrap_or_else(|| format!("condition on `{field}` not met")),
            );
            err.details = Some(json!({ "effect": index, "field": field }));
            err
        }
        ApplyError::Precondition { field, index, .. } => {
            let mut err = CrudError::new(
                409,
                "precondition_failed",
                format!("condition on `{field}` not met"),
            );
            err.details = Some(json!({ "effect": index, "field": field }));
            err
        }
        ApplyError::NotFound { collection, id, .. } => {
            CrudError::new(404, "not_found", format!("{collection}/{id} not found"))
        }
        ApplyError::Conflict { collection, id, .. } => {
            CrudError::new(409, "conflict", format!("{collection}/{id} already exists"))
        }
        ApplyError::VersionMismatch {
            expected, actual, ..
        } => {
            let mut err = CrudError::new(412, "precondition_failed", format!("the document changed (you have version {expected}, it is at {actual}); re-read it and retry"));
            err.headers.push(("etag".into(), format!("\"{actual}\"")));
            err
        }
        ApplyError::Invalid { .. } => CrudError::new(
            500,
            "effect_invalid",
            format!("a write could not be applied: {e}"),
        ),
        ApplyError::Backend(inner) => {
            tracing::error!(error = %format!("{inner:#}"), "database error");
            CrudError::new(500, "database_error", "the database request failed")
        }
    }
}

fn db_err(e: anyhow::Error) -> CrudError {
    tracing::error!(error = %format!("{e:#}"), "database error");
    CrudError::new(500, "database_error", "the database request failed")
}

fn bad(code: &str, msg: impl Into<String>) -> CrudError {
    CrudError::new(400, code, msg)
}

/// Data rules plus what they need to know about this request.
pub struct RuleInput<'a> {
    pub rules: &'a DataRules,
    /// The signed-in subject (needed when the rules scope data to an owner).
    pub subject: Option<&'a str>,
    /// Template variables: `body`, `query`, `path`, `identity`, `request_id`, `route`
    /// (`id` and `now` are added here).
    pub vars: &'a Value,
}

/// What the HTTP layer hands over.
pub struct CrudInput<'a> {
    /// Value of the route's id path parameter.
    pub path_id: Option<&'a str>,
    /// Parsed JSON request body.
    pub body: Option<&'a Value>,
    /// Query string pairs, in order.
    pub query: &'a [(String, String)],
    /// Query parameters the OpenAPI operation declares: only these act as list filters.
    pub declared_query: &'a [&'a str],
    /// Raw `If-Match` header.
    pub if_match: Option<&'a str>,
    /// Request path, for the `Location` header.
    pub path: &'a str,
    pub rules: Option<RuleInput<'a>>,
}

#[derive(Debug, Default)]
pub struct CrudOutput {
    pub body: Option<Value>,
    pub headers: Vec<(String, String)>,
}

/// `"3"`, `W/"3"` or `3` → `Some(3)`; `*` → `None` (any version); anything else is a 400.
pub fn parse_if_match(raw: &str) -> Result<Option<u64>, CrudError> {
    let t = raw.trim();
    if t == "*" {
        return Ok(None);
    }
    t.trim_start_matches("W/")
        .trim_matches('"')
        .parse::<u64>()
        .map(Some)
        .map_err(|_| bad("invalid_if_match", "If-Match must be a single document version, e.g. \"3\" (as returned in ETag), or *"))
}

fn id_value(kind: IdKind, raw: &str) -> Result<Value, CrudError> {
    match kind {
        IdKind::Integer => raw
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| bad("invalid_id", format!("`{raw}` is not an integer id"))),
        IdKind::String => Ok(Value::String(raw.to_string())),
    }
}

fn body_object(input: &CrudInput) -> Result<Map<String, Value>, CrudError> {
    match input.body {
        Some(Value::Object(m)) => Ok(m.clone()),
        _ => Err(bad("invalid_body", "a JSON object body is required")),
    }
}

fn etag(version: u64) -> (String, String) {
    ("etag".into(), format!("\"{version}\""))
}

/// Execute the planned operation. `pending` are the effects scripts queued before this point: they
/// run in the **same transaction** as the route's own write, ahead of it, so either everything
/// happens or nothing does. Data rules (`rules`) compile down to the same machinery: extra effects,
/// conditions under row locks, and list filters.
pub async fn run(
    store: &dyn DocStore,
    plan: &CrudPlan,
    input: &CrudInput<'_>,
    pending: Vec<Effect>,
) -> Result<CrudOutput, CrudError> {
    let coll = plan.collection.as_str();
    let rules: Option<&DataRules> = input
        .rules
        .as_ref()
        .map(|r| r.rules)
        .filter(|r| !r.is_empty());
    let subject = input.rules.as_ref().and_then(|r| r.subject);
    if rules.is_some_and(|r| r.owner.is_some()) && subject.is_none() {
        return Err(CrudError::new(
            401,
            "authentication_required",
            "this route is scoped to the signed-in user; sign in first",
        ));
    }
    let now = rules::now_iso();
    let path_id = || -> Result<String, CrudError> {
        input.path_id.map(String::from).ok_or_else(|| {
            bad(
                "invalid_id",
                format!(
                    "missing path parameter `{}`",
                    plan.id_param.as_deref().unwrap_or("id")
                ),
            )
        })
    };
    let expect = || -> Result<Option<u64>, CrudError> {
        input
            .if_match
            .map(parse_if_match)
            .transpose()
            .map(Option::flatten)
    };
    let hide = |d: &mut Value| {
        if let Some(r) = rules {
            r.hide_in(d);
        }
    };
    // template variables for this document
    let vars_for = |id: &str| -> Value {
        let mut v = input
            .rules
            .as_ref()
            .map(|r| r.vars.clone())
            .unwrap_or_else(|| json!({}));
        if let Some(o) = v.as_object_mut() {
            o.insert(
                "id".into(),
                id_value(plan.id_kind, id).unwrap_or_else(|_| json!(id)),
            );
            o.insert("now".into(), json!(now));
        }
        v
    };
    // an existing document must belong to the caller and not be soft-deleted, else "not found"
    let guarded = |mut e: Effect| -> Effect {
        if let Some(r) = rules {
            let g = r.guard(subject);
            if !g.is_empty() {
                e.on_fail = Some(Failure {
                    status: 404,
                    code: "not_found".into(),
                    message: Some(format!(
                        "{coll}/{} not found",
                        e.id.clone().unwrap_or_default()
                    )),
                });
                e.require = Some(g);
            }
        }
        e
    };

    // Reads still honour queued effects (e.g. an audit row for "viewed"), committed first.
    if matches!(plan.op, Op::Read | Op::List | Op::None) && !pending.is_empty() {
        store.apply(&pending).await.map_err(|e| http_error(&e))?;
    }

    let (main, op_name, doc_id): (Effect, &str, String) = match plan.op {
        Op::None => return Ok(CrudOutput::default()),
        Op::Read => {
            let id = path_id()?;
            let (mut doc, version) = store
                .get_versioned(coll, &id)
                .await
                .map_err(db_err)?
                .ok_or_else(|| {
                    CrudError::new(404, "not_found", format!("{coll}/{id} not found"))
                })?;
            if let Some(r) = rules {
                if crate::effects::check_require(Some(&doc), &r.guard(subject)).is_err() {
                    return Err(CrudError::new(
                        404,
                        "not_found",
                        format!("{coll}/{id} not found"),
                    ));
                }
            }
            hide(&mut doc);
            return Ok(CrudOutput {
                body: Some(doc),
                headers: vec![etag(version)],
            });
        }
        Op::List => {
            let mut q = ListQuery {
                limit: 100,
                ..Default::default()
            };
            for (k, v) in input.query {
                match k.as_str() {
                    "limit" => {
                        q.limit = v
                            .parse::<usize>()
                            .ok()
                            .filter(|n| (1..=1000).contains(n))
                            .ok_or_else(|| bad("invalid_query", "limit must be 1..=1000"))?
                    }
                    "offset" => {
                        q.offset = v.parse().map_err(|_| {
                            bad("invalid_query", "offset must be a non-negative integer")
                        })?
                    }
                    "sort" => {
                        let (f, desc) = v
                            .strip_prefix('-')
                            .map(|f| (f, true))
                            .unwrap_or((v.as_str(), false));
                        if !valid_field(f) {
                            return Err(bad(
                                "invalid_query",
                                "sort must be a field name, optionally prefixed with `-`",
                            ));
                        }
                        q.sort = Some((f.to_string(), desc));
                    }
                    other if input.declared_query.contains(&other) && valid_field(other) => {
                        q.filters.push((other.to_string(), v.clone()))
                    }
                    _ => {}
                }
            }
            if let Some(r) = rules {
                if let (Some(o), Some(sub)) = (&r.owner, subject) {
                    q.filters.retain(|(f, _)| f != o); // a client cannot widen or redirect the owner scope
                    q.filters.push((o.clone(), sub.to_string()));
                }
                if let Some(flag) = &r.soft_delete {
                    q.ne_filters.push((flag.clone(), "true".into()));
                }
            }
            let total = store.count_matching(coll, &q).await.map_err(db_err)?;
            let mut docs = store.list(coll, &q).await.map_err(db_err)?;
            docs.iter_mut().for_each(hide);
            return Ok(CrudOutput {
                body: Some(Value::Array(docs)),
                headers: vec![("x-total-count".into(), total.to_string())],
            });
        }
        Op::Create => {
            let mut doc = body_object(input)?;
            if let Some(r) = rules {
                r.shape_create(&mut doc, subject, &now);
            }
            let id = match doc.get(&plan.id_field).filter(|v| !v.is_null()) {
                Some(Value::Number(n)) if plan.id_kind == IdKind::Integer && n.is_i64() => {
                    let id = n.as_i64().unwrap_or_default();
                    store.bump_sequence(coll, id).await.map_err(db_err)?;
                    id.to_string()
                }
                Some(Value::String(s)) if plan.id_kind == IdKind::String && !s.is_empty() => {
                    s.clone()
                }
                Some(_) => {
                    return Err(bad(
                        "invalid_id",
                        format!(
                            "`{}` must be {}",
                            plan.id_field,
                            match plan.id_kind {
                                IdKind::Integer => "an integer",
                                IdKind::String => "a non-empty string",
                            }
                        ),
                    ))
                }
                None => match plan.id_kind {
                    IdKind::Integer => {
                        let id = store.next_sequence(coll).await.map_err(db_err)?;
                        doc.insert(plan.id_field.clone(), json!(id));
                        id.to_string()
                    }
                    IdKind::String => {
                        let id = uuid::Uuid::new_v4().to_string();
                        doc.insert(plan.id_field.clone(), json!(id));
                        id
                    }
                },
            };
            (Effect::insert(coll, &id, Value::Object(doc)), "create", id)
        }
        Op::Replace => {
            let id = path_id()?;
            let mut doc = body_object(input)?;
            doc.insert(plan.id_field.clone(), id_value(plan.id_kind, &id)?); // the URL wins over the body
            let mut e =
                Effect::replace(coll, &id, Value::Object(doc)).with_expect_version(expect()?);
            if let Some(r) = rules {
                if let Some(Value::Object(d)) = e.doc.as_mut() {
                    if let Some(u) = r.is_stamped_updated() {
                        d.insert(u.to_string(), json!(now));
                    }
                }
                e.keep = Some(r.keep_on_replace()).filter(|k| !k.is_empty());
            }
            (guarded(e), "replace", id)
        }
        Op::Merge => {
            let id = path_id()?;
            let mut patch = body_object(input)?;
            patch.remove(&plan.id_field); // ids are immutable
            if let Some(r) = rules {
                for f in r.protected() {
                    patch.remove(&f);
                }
                if let Some(u) = r.is_stamped_updated() {
                    patch.insert(u.to_string(), json!(now));
                }
            }
            (
                guarded(
                    Effect::merge_patch(coll, &id, Value::Object(patch))
                        .with_expect_version(expect()?),
                ),
                "merge",
                id,
            )
        }
        Op::Delete => {
            let id = path_id()?;
            let e = match rules.and_then(|r| r.soft_delete.as_ref().map(|f| (r, f))) {
                Some((r, flag)) => {
                    let mut set = Map::new();
                    set.insert(flag.clone(), json!(true));
                    if let Some(t) = &r.timestamps {
                        for f in [&t.deleted, &t.updated].into_iter().flatten() {
                            set.insert(f.clone(), json!(now));
                        }
                    }
                    serde_json::from_value::<Effect>(
                        json!({"op": "update", "collection": coll, "id": id, "set": set}),
                    )
                    .map_err(|e| db_err(e.into()))?
                    .with_expect_version(expect()?)
                }
                None => Effect::delete(coll, &id).with_expect_version(expect()?),
            };
            (guarded(e), "delete", id)
        }
    };

    // [rule effects] [script effects] [the route's own write] [audit row]
    let mut all: Vec<Effect> = vec![];
    if let Some(r) = rules {
        all.extend(
            r.render_effects(&vars_for(&doc_id))
                .map_err(|m| bad("invalid_request", m))?,
        );
    }
    all.extend(pending);
    let main_index = all.len();
    all.push(main);
    if let Some(r) = rules {
        all.extend(r.audit_effect(&vars_for(&doc_id), op_name, coll));
    }
    let results: Vec<OpResult> = store.apply(&all).await.map_err(|e| http_error(&e))?;
    let r = &results[main_index];
    let mut headers = vec![];
    if let Some(v) = r.version {
        headers.push(etag(v));
    }
    if plan.op == Op::Create {
        headers.push((
            "location".into(),
            format!("{}/{}", input.path.trim_end_matches('/'), pct(&r.id)),
        ));
    }
    let mut body = r.doc.clone();
    // a soft delete is a DELETE to the client: no body, no etag
    if plan.op == Op::Delete {
        body = None;
        headers.clear();
    }
    if let Some(b) = body.as_mut() {
        hide(b);
    }
    Ok(CrudOutput { body, headers })
}

/// Minimal percent-encoding for a single path segment.
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod exec_tests {
    use super::*;

    #[test]
    fn if_match_parsing() {
        assert_eq!(parse_if_match("\"3\"").unwrap(), Some(3));
        assert_eq!(parse_if_match("W/\"12\"").unwrap(), Some(12));
        assert_eq!(parse_if_match(" 7 ").unwrap(), Some(7));
        assert_eq!(parse_if_match("*").unwrap(), None);
        assert_eq!(
            parse_if_match("\"a\"").unwrap_err().code,
            "invalid_if_match"
        );
        assert_eq!(
            parse_if_match("\"1\", \"2\"").unwrap_err().code,
            "invalid_if_match"
        );
    }

    #[test]
    fn errors_map_to_http() {
        let f = crate::effects::Failure {
            status: 409,
            code: "out_of_stock".into(),
            message: Some("sold out".into()),
        };
        let e = http_error(&ApplyError::Precondition {
            index: 1,
            field: "stock".into(),
            on_fail: Some(f),
        });
        assert_eq!(
            (e.status, e.code.as_str(), e.message.as_str()),
            (409, "out_of_stock", "sold out")
        );
        assert_eq!(e.details.unwrap()["field"], "stock");
        assert_eq!(
            http_error(&ApplyError::Precondition {
                index: 0,
                field: "x".into(),
                on_fail: None
            })
            .code,
            "precondition_failed"
        );
        assert_eq!(
            http_error(&ApplyError::NotFound {
                index: 0,
                collection: "pets".into(),
                id: "9".into()
            })
            .status,
            404
        );
        assert_eq!(
            http_error(&ApplyError::Conflict {
                index: 0,
                collection: "pets".into(),
                id: "9".into()
            })
            .status,
            409
        );
        let v = http_error(&ApplyError::VersionMismatch {
            index: 0,
            expected: 2,
            actual: 5,
        });
        assert_eq!(
            (v.status, v.headers[0].1.as_str()),
            (412, "\"5\""),
            "a 412 carries the current ETag"
        );
        assert_eq!(
            http_error(&ApplyError::Invalid {
                index: 0,
                message: "boom".into()
            })
            .status,
            500
        );
        let b = http_error(&ApplyError::Backend(anyhow::anyhow!(
            "secret connection string"
        )));
        assert!(
            !b.message.contains("secret"),
            "backend details never reach clients"
        );
        assert_eq!(pct("a b/ü"), "a%20b%2F%C3%BC");
    }
}

#[cfg(test)]
mod infer_tests {
    use super::*;

    fn plan(
        method: &str,
        path: &str,
        has_body: bool,
        schema: Option<Value>,
        over: CrudConfig,
    ) -> Result<Option<CrudPlan>> {
        infer(
            &Shape {
                method,
                path,
                has_body,
                response_schema: schema.as_ref(),
                id_param_type: None,
            },
            &over,
        )
    }
    fn p(method: &str, path: &str, has_body: bool, schema: Option<Value>) -> Option<CrudPlan> {
        plan(method, path, has_body, schema, CrudConfig::default()).unwrap()
    }

    #[test]
    fn infers_resource_operations() {
        let arr = Some(
            json!({"type":"array","items":{"type":"object","properties":{"id":{"type":"integer"}}}}),
        );
        let item = Some(
            json!({"allOf":[{"type":"object","properties":{"name":{"type":"string"}}},{"properties":{"id":{"type":"integer"}}}]}),
        );
        let l = p("GET", "/pets", false, arr).unwrap();
        assert_eq!(
            (l.op, l.collection.as_str(), l.id_kind),
            (Op::List, "pets", IdKind::Integer)
        );
        let c = p("POST", "/pets", true, item.clone()).unwrap();
        assert_eq!(
            (c.op, c.id_kind),
            (Op::Create, IdKind::Integer),
            "id type found through allOf"
        );
        let r = p("GET", "/pets/{petId}", false, item.clone()).unwrap();
        assert_eq!((r.op, r.id_param.as_deref()), (Op::Read, Some("petId")));
        assert_eq!(
            p("PUT", "/pets/{id}", true, item.clone()).unwrap().op,
            Op::Replace
        );
        assert_eq!(
            p("PATCH", "/pets/{id}", true, item.clone()).unwrap().op,
            Op::Merge
        );
        assert_eq!(
            p("DELETE", "/pets/{id}", false, None).unwrap().op,
            Op::Delete
        );
        assert_eq!(
            p("GET", "/owners/{o}/pets/{id}", false, None)
                .unwrap()
                .collection,
            "pets",
            "last static segment"
        );
        let s = p(
            "GET",
            "/users/{id}",
            false,
            Some(json!({"properties":{"id":{"type":"string","format":"uuid"}}})),
        )
        .unwrap();
        assert_eq!(s.id_kind, IdKind::String);
    }

    #[test]
    fn non_resources_are_left_alone() {
        assert!(
            p("GET", "/health", false, Some(json!({"type":"object"}))).is_none(),
            "singleton object GET is not a list"
        );
        assert!(p("POST", "/pets/{id}/adopt", false, None).is_none());
        assert!(
            p("POST", "/login", false, None).is_none(),
            "POST without a body"
        );
        assert!(
            p("GET", "/pets", false, None).is_none(),
            "no response schema: do not guess"
        );
    }

    #[test]
    fn overrides_and_errors() {
        let over = CrudConfig {
            collection: Some("animals".into()),
            operation: Some(Op::Create),
            id_kind: Some(IdKind::String),
            ..Default::default()
        };
        let c = plan("POST", "/pets/{id}/adopt", false, None, over)
            .unwrap()
            .unwrap();
        assert_eq!(
            (c.op, c.collection.as_str(), c.id_kind),
            (Op::Create, "animals", IdKind::String)
        );
        let off = CrudConfig {
            operation: Some(Op::None),
            ..Default::default()
        };
        assert!(
            plan("GET", "/pets/{id}", false, None, off)
                .unwrap()
                .is_none(),
            "explicit opt-out"
        );
        assert!(
            plan(
                "GET",
                "/",
                false,
                None,
                CrudConfig {
                    operation: Some(Op::List),
                    ..Default::default()
                }
            )
            .is_err(),
            "no collection derivable"
        );
        assert!(
            plan(
                "GET",
                "/pets",
                false,
                None,
                CrudConfig {
                    operation: Some(Op::Read),
                    ..Default::default()
                }
            )
            .is_err(),
            "read needs an id"
        );
        assert!(plan(
            "POST",
            "/pets",
            true,
            None,
            CrudConfig {
                collection: Some("a b".into()),
                ..Default::default()
            }
        )
        .is_err());
    }
}
