//! A small, GraphQL-shaped read language: ask for exactly the fields you want, follow relations,
//! order and page, all in one request.
//!
//! ```json
//! { "from": "orders",
//!   "where": { "status": { "in": ["open", "paid"] }, "total": { "gte": 100 } },
//!   "select": [ "id", "total",
//!     { "items":    { "from": "orderItems", "by": "orderId", "select": ["sku", "qty"] } },
//!     { "customer": { "from": "customers",  "id": "customerId", "select": ["name"] } } ],
//!   "orderBy": [ { "createdAt": "desc" } ], "limit": 20, "total": true }
//! ```
//!
//! * `by` (has-many): children whose `by` field equals the parent's id.
//!   `id` (belongs-to): the parent's field that holds the child's id.
//! * Relations can be named once in configuration and referenced as `{ "items": "items" }`.
//! * **No N+1:** each relation is resolved with one batched query per level, however many parent
//!   rows there are.
//! * `where` uses exactly the operators of effect preconditions (`eq ne gt gte lt lte in nin exists`)
//!   and the same evaluator, so every backend agrees. Plain equality is pushed down to the database;
//!   anything else scans the collection (capped; the query fails clearly instead of getting slow).

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    future::Future,
    pin::Pin,
};

use anyhow::Result as AnyResult;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{
    effects::{check_require, get_path, validate_require},
    valid_collection, valid_field, DocStore, ListQuery,
};

/// `collection -> relation name -> definition`, from configuration.
pub type Relations = BTreeMap<String, BTreeMap<String, RelationDef>>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationDef {
    /// The related collection.
    pub from: String,
    /// Has-many: the field *in the related documents* that holds this document's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Belongs-to: the field *in this document* that holds the related document's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl RelationDef {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_collection(&self.from) {
            return Err(format!(
                "relation target `{}` is not a valid collection name",
                self.from
            ));
        }
        match (&self.by, &self.id) {
            (Some(f), None) | (None, Some(f)) if valid_field(f) => Ok(()),
            (Some(_), None) | (None, Some(_)) => {
                Err("relation field is not a valid field path".into())
            }
            _ => Err("a relation needs exactly one of `by` (has-many) or `id` (belongs-to)".into()),
        }
    }
}

pub fn validate_relations(r: &Relations) -> Result<(), String> {
    for (coll, rels) in r {
        if !valid_collection(coll) {
            return Err(format!(
                "relations: `{coll}` is not a valid collection name"
            ));
        }
        for (name, def) in rels {
            if !valid_field(name) || name.contains('.') {
                return Err(format!(
                    "relations.{coll}: `{name}` is not a valid relation name"
                ));
            }
            def.validate()
                .map_err(|e| format!("relations.{coll}.{name}: {e}"))?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_depth: usize,
    /// Most documents a single level may have to examine when it cannot be answered by the database.
    pub max_scan: usize,
    /// Most documents one query may load in total, across all levels.
    pub max_docs: usize,
    pub default_limit: usize,
    pub max_limit: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 4,
            max_scan: 10_000,
            max_docs: 5_000,
            default_limit: 100,
            max_limit: 1_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Level {
    pub from: String,
    /// Root only: fetch the document with this row id.
    pub id: Option<String>,
    pub where_: Option<Map<String, Value>>,
    /// `None` = every field.
    pub select: Option<Vec<Item>>,
    pub order_by: Vec<(String, bool)>,
    pub limit: Option<usize>,
    pub offset: usize,
    pub total: bool,
}

pub type Query = Level;

#[derive(Debug, Clone)]
pub enum Item {
    All,
    Field(String),
    Rel(Rel),
}

#[derive(Debug, Clone)]
pub struct Rel {
    pub name: String,
    pub def: RelationDef,
    pub level: Level,
}

// ------------------------------------------------------------------ parsing

fn only_keys(o: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), String> {
    match o.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!(
            "{what}: unknown key `{k}` (allowed: {})",
            allowed.join(", ")
        )),
        None => Ok(()),
    }
}

fn parse_where(v: Option<&Value>) -> Result<Option<Map<String, Value>>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => {
            validate_require(m).map_err(|e| format!("where: {e}"))?;
            Ok(Some(m.clone()))
        }
        Some(_) => Err(
            "where must be an object like { \"status\": \"open\", \"total\": { \"gte\": 10 } }"
                .into(),
        ),
    }
}

fn parse_order_by(v: Option<&Value>) -> Result<Vec<(String, bool)>, String> {
    let item = |v: &Value| -> Result<(String, bool), String> {
        let (field, desc) =
            match v {
                Value::String(s) => match s.strip_prefix('-') {
                    Some(f) => (f.to_string(), true),
                    None => (s.trim_start_matches('+').to_string(), false),
                },
                Value::Object(o) if o.len() == 1 => {
                    let (k, d) = o.iter().next().expect("len is 1");
                    match d.as_str() {
                        Some("asc") => (k.clone(), false),
                        Some("desc") => (k.clone(), true),
                        _ => return Err(format!("orderBy.{k} must be \"asc\" or \"desc\"")),
                    }
                }
                _ => return Err(
                    "orderBy entries are \"field\", \"-field\" or { \"field\": \"asc\"|\"desc\" }"
                        .into(),
                ),
            };
        if !valid_field(&field) {
            return Err(format!("orderBy: `{field}` is not a valid field path"));
        }
        Ok((field, desc))
    };
    match v {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) => a.iter().map(item).collect(),
        Some(single) => Ok(vec![item(single)?]),
    }
}

fn parse_usize(v: Option<&Value>, what: &str) -> Result<Option<usize>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(n) => n
            .as_u64()
            .map(|n| Some(n as usize))
            .ok_or_else(|| format!("{what} must be a non-negative integer")),
    }
}

fn lookup<'a>(rels: &'a Relations, parent: &str, name: &str) -> Result<&'a RelationDef, String> {
    rels.get(parent).and_then(|m| m.get(name)).ok_or_else(|| {
        let known: Vec<&str> = rels
            .get(parent)
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();
        format!("no relation `{name}` is configured for `{parent}` (configured: {known:?})")
    })
}

fn parse_select(
    v: Option<&Value>,
    parent: &str,
    rels: &Relations,
    depth: usize,
    max_depth: usize,
) -> Result<Option<Vec<Item>>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let arr = v
        .as_array()
        .ok_or("select must be an array of fields and relations")?;
    let mut items = Vec::new();
    for e in arr {
        match e {
            Value::String(s) if s == "*" => items.push(Item::All),
            Value::String(s) if valid_field(s) => items.push(Item::Field(s.clone())),
            Value::String(s) => return Err(format!("select: `{s}` is not a valid field path")),
            Value::Object(o) if o.len() == 1 => {
                let (name, spec) = o.iter().next().expect("len is 1");
                if !valid_field(name) || name.contains('.') {
                    return Err(format!("select: `{name}` is not a valid relation name"));
                }
                items.push(Item::Rel(parse_rel(
                    name,
                    spec,
                    parent,
                    rels,
                    depth + 1,
                    max_depth,
                )?));
            }
            _ => {
                return Err(
                    "select entries are field names, \"*\", or { \"name\": <relation> }".into(),
                )
            }
        }
    }
    Ok(Some(items))
}

fn parse_rel(
    name: &str,
    spec: &Value,
    parent: &str,
    rels: &Relations,
    depth: usize,
    max_depth: usize,
) -> Result<Rel, String> {
    if depth > max_depth {
        return Err(format!(
            "relations are nested deeper than {max_depth} levels"
        ));
    }
    let ctx = |e: String| format!("relation `{name}`: {e}");
    let (def, level_spec) = match spec {
        Value::String(named) => (lookup(rels, parent, named).map_err(ctx)?.clone(), None),
        Value::Object(o) => {
            only_keys(
                o,
                &[
                    "rel", "from", "by", "id", "where", "select", "orderBy", "limit",
                ],
                &format!("relation `{name}`"),
            )
            .map_err(|e| e.to_string())?;
            let def = match o.get("rel") {
                Some(Value::String(named)) => {
                    if ["from", "by", "id"].iter().any(|k| o.contains_key(*k)) {
                        return Err(ctx("`rel` cannot be combined with from/by/id".into()));
                    }
                    lookup(rels, parent, named).map_err(ctx)?.clone()
                }
                Some(_) => return Err(ctx("`rel` must be a relation name".into())),
                None => {
                    let s = |k: &str| {
                        o.get(k)
                            .map(|v| {
                                v.as_str()
                                    .map(String::from)
                                    .ok_or_else(|| ctx(format!("`{k}` must be a string")))
                            })
                            .transpose()
                    };
                    let def = RelationDef {
                        from: s("from")?.ok_or_else(|| ctx("needs `from` (or `rel`)".into()))?,
                        by: s("by")?,
                        id: s("id")?,
                    };
                    def.validate().map_err(ctx)?;
                    def
                }
            };
            (def, Some(o))
        }
        _ => return Err(ctx("must be a relation name or an object".into())),
    };
    let (where_, select, order_by, limit) = match level_spec {
        Some(o) => (
            parse_where(o.get("where")).map_err(ctx)?,
            parse_select(o.get("select"), &def.from, rels, depth, max_depth).map_err(ctx)?,
            parse_order_by(o.get("orderBy")).map_err(ctx)?,
            parse_usize(o.get("limit"), "limit").map_err(ctx)?,
        ),
        None => (None, None, vec![], None),
    };
    let level = Level {
        from: def.from.clone(),
        id: None,
        where_,
        select,
        order_by,
        limit,
        offset: 0,
        total: false,
    };
    Ok(Rel {
        name: name.to_string(),
        def,
        level,
    })
}

/// Parse and validate a query. All structural problems are reported here, before any database access.
pub fn parse_query(v: &Value, rels: &Relations, max_depth: usize) -> Result<Query, String> {
    let o = v
        .as_object()
        .ok_or("a query is a JSON object: { \"from\": \"orders\", … }")?;
    only_keys(
        o,
        &[
            "from", "id", "where", "select", "orderBy", "limit", "offset", "total",
        ],
        "query",
    )?;
    let from = o
        .get("from")
        .and_then(Value::as_str)
        .ok_or("query needs `from` (a collection name)")?
        .to_string();
    if !valid_collection(&from) {
        return Err(format!("`{from}` is not a valid collection name"));
    }
    let id = match o.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Some(n.to_string()),
        Some(_) => return Err("id must be a non-empty string or an integer".into()),
    };
    if id.is_some()
        && (o.contains_key("limit")
            || o.contains_key("offset")
            || o.contains_key("orderBy")
            || o.contains_key("total"))
    {
        return Err(
            "a query by `id` returns one document; limit/offset/orderBy/total do not apply".into(),
        );
    }
    Ok(Level {
        select: parse_select(o.get("select"), &from, rels, 0, max_depth)?,
        where_: parse_where(o.get("where"))?,
        order_by: parse_order_by(o.get("orderBy"))?,
        limit: parse_usize(o.get("limit"), "limit")?,
        offset: parse_usize(o.get("offset"), "offset")?.unwrap_or(0),
        total: o
            .get("total")
            .map(|t| t.as_bool().ok_or("total must be true or false"))
            .transpose()?
            .unwrap_or(false),
        from,
        id,
    })
}

// ------------------------------------------------------------------ execution

#[derive(Debug)]
pub enum QueryError {
    Invalid(String),
    /// A collection the caller may not read.
    Denied(String),
    /// Would examine or load more documents than the limits allow.
    TooBroad(String),
    Backend(anyhow::Error),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(m) | Self::Denied(m) | Self::TooBroad(m) => write!(f, "{m}"),
            Self::Backend(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<anyhow::Error> for QueryError {
    fn from(e: anyhow::Error) -> Self {
        Self::Backend(e)
    }
}

pub fn http_error(e: &QueryError) -> crate::crud::CrudError {
    use crate::crud::CrudError;
    match e {
        QueryError::Invalid(m) => CrudError::new(400, "invalid_query", m.clone()),
        QueryError::Denied(m) => CrudError::new(403, "forbidden", m.clone()),
        QueryError::TooBroad(m) => CrudError::new(422, "query_too_broad", m.clone()),
        QueryError::Backend(inner) => {
            tracing::error!(error = %format!("{inner:#}"), "database error");
            CrudError::new(500, "database_error", "the database request failed")
        }
    }
}

type Row = (String, Value);
type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct Engine<'a> {
    pub store: &'a dyn DocStore,
    pub limits: Limits,
    /// Called for every collection the query touches; return a message to refuse.
    pub can_read: &'a (dyn Fn(&str) -> Result<(), String> + Send + Sync),
}

/// JSON ordering used for `orderBy`: missing and null first, then booleans, numbers, strings.
fn rank(v: Option<&Value>) -> u8 {
    match v {
        None | Some(Value::Null) => 0,
        Some(Value::Bool(_)) => 1,
        Some(Value::Number(_)) => 2,
        Some(Value::String(_)) => 3,
        Some(Value::Array(_)) => 4,
        Some(Value::Object(_)) => 5,
    }
}

pub fn cmp_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering::Equal;
    match rank(a).cmp(&rank(b)) {
        Equal => match (a, b) {
            (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
            (Some(Value::Number(x)), Some(Value::Number(y))) => {
                x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(Equal)
            }
            (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
            (Some(x @ (Value::Array(_) | Value::Object(_))), Some(y)) => {
                x.to_string().cmp(&y.to_string())
            }
            _ => Equal,
        },
        other => other,
    }
}

fn sort_rows(rows: &mut [Row], order: &[(String, bool)]) {
    if order.is_empty() {
        return;
    }
    rows.sort_by(|a, b| {
        for (field, desc) in order {
            let o = cmp_values(get_path(&a.1, field), get_path(&b.1, field));
            let o = if *desc { o.reverse() } else { o };
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        std::cmp::Ordering::Equal
    }); // `sort_by` is stable: ties keep insertion order
}

/// Text form used to match ids and foreign keys (so `1` and `"1"` join).
fn key_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Equality conditions the database can evaluate for us; everything else is checked in Rust.
fn pushdown(w: &Map<String, Value>) -> Vec<(String, String)> {
    w.iter()
        .filter_map(|(field, spec)| {
            let scalar = match spec {
                Value::Object(o) if o.len() == 1 => o.get("eq")?,
                other => other,
            };
            let text = match scalar {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
                _ => return None, // floats, null, arrays: decided in Rust
            };
            Some((field.clone(), text))
        })
        .collect()
}

fn insert_path(out: &mut Map<String, Value>, path: &str, value: Value) {
    let mut segs: Vec<&str> = path.split('.').collect();
    let last = segs.pop().expect("split yields one segment");
    let mut cur = out;
    for s in segs {
        let next = cur
            .entry(s.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        match next {
            Value::Object(m) => cur = m,
            _ => return,
        }
    }
    cur.insert(last.to_string(), value);
}

impl<'a> Engine<'a> {
    pub async fn run(&self, q: &Query) -> Result<Value, QueryError> {
        let mut budget = self.limits.max_docs;
        if let Some(id) = &q.id {
            self.readable(&q.from)?;
            let mut rows = self
                .store
                .get_many(&q.from, std::slice::from_ref(id))
                .await?;
            self.charge(&mut budget, rows.len())?;
            rows.retain(|(_, d)| {
                q.where_
                    .as_ref()
                    .is_none_or(|w| check_require(Some(d), w).is_ok())
            });
            let out = self.resolve(q, rows, &mut budget, 0).await?;
            return Ok(json!({ "data": out.into_iter().next().unwrap_or(Value::Null) }));
        }
        let (rows, total) = self.fetch(q, None, &mut budget).await?;
        let out = self.resolve(q, rows, &mut budget, 0).await?;
        let mut res = json!({ "data": out });
        if let Some(t) = total.filter(|_| q.total) {
            res["total"] = json!(t);
        }
        Ok(res)
    }

    fn readable(&self, collection: &str) -> Result<(), QueryError> {
        (self.can_read)(collection).map_err(QueryError::Denied)
    }

    fn charge(&self, budget: &mut usize, n: usize) -> Result<(), QueryError> {
        if n > *budget {
            return Err(QueryError::TooBroad(format!(
                "the query would load more than {} documents; narrow it with where/limit",
                self.limits.max_docs
            )));
        }
        *budget -= n;
        Ok(())
    }

    /// Rows of one level. For the root (`link = None`) the page (offset/limit) is applied; for a
    /// relation level (`link = Some((field, parent keys))`) all matches are returned sorted, and the
    /// caller applies the per-parent limit.
    async fn fetch(
        &self,
        lv: &Level,
        link: Option<(&str, Vec<String>)>,
        budget: &mut usize,
    ) -> Result<(Vec<Row>, Option<u64>), QueryError> {
        self.readable(&lv.from)?;
        let eq = lv.where_.as_ref().map(pushdown).unwrap_or_default();
        let only_pushdown = lv.where_.as_ref().is_none_or(|w| w.len() == eq.len());
        let limit = lv
            .limit
            .unwrap_or(self.limits.default_limit)
            .clamp(1, self.limits.max_limit);

        // Direct path: the database sorts and pages (equality filters, at most one sort key).
        if link.is_none() && only_pushdown && lv.order_by.len() <= 1 {
            let q = ListQuery {
                filters: eq.clone(),
                in_filters: vec![],
                ne_filters: vec![],
                sort: lv.order_by.first().cloned(),
                limit,
                offset: lv.offset,
            };
            let rows = self.store.list_rows(&lv.from, &q).await?;
            self.charge(budget, rows.len())?;
            let total = if lv.total {
                Some(self.store.count(&lv.from, &eq).await?)
            } else {
                None
            };
            return Ok((rows, total));
        }

        // Scan path: fetch the candidates the database can narrow down, then filter and sort here.
        let in_filters = link
            .map(|(f, keys)| vec![(f.to_string(), keys)])
            .unwrap_or_default();
        let is_link = !in_filters.is_empty();
        let q = ListQuery {
            filters: eq,
            in_filters,
            ne_filters: vec![],
            sort: None,
            limit: self.limits.max_scan + 1,
            offset: 0,
        };
        let mut rows = self.store.list_rows(&lv.from, &q).await?;
        if rows.len() > self.limits.max_scan {
            return Err(QueryError::TooBroad(format!(
                "this query on `{}` has to examine more than {} documents; add an equality filter on a field to narrow it",
                lv.from, self.limits.max_scan
            )));
        }
        if let Some(w) = &lv.where_ {
            rows.retain(|(_, d)| check_require(Some(d), w).is_ok());
        }
        sort_rows(&mut rows, &lv.order_by);
        let total = Some(rows.len() as u64);
        if !is_link {
            rows = rows.into_iter().skip(lv.offset).take(limit).collect();
        }
        self.charge(budget, rows.len())?;
        Ok((rows, total))
    }

    /// Project `rows` and attach relations, one batched query per relation.
    fn resolve<'b>(
        &'b self,
        lv: &'b Level,
        rows: Vec<Row>,
        budget: &'b mut usize,
        depth: usize,
    ) -> BoxFut<'b, Result<Vec<Value>, QueryError>> {
        Box::pin(async move {
            let items = lv.select.as_deref();
            // relation name -> per-parent-row value
            let mut attached: Vec<(String, Vec<Value>)> = Vec::new();
            for it in items.unwrap_or_default() {
                let Item::Rel(rel) = it else { continue };
                if depth >= self.limits.max_depth {
                    return Err(QueryError::Invalid(format!(
                        "relations are nested deeper than {} levels",
                        self.limits.max_depth
                    )));
                }
                attached.push((
                    rel.name.clone(),
                    self.attach(rel, &rows, budget, depth + 1).await?,
                ));
            }
            let mut out = Vec::with_capacity(rows.len());
            for (i, (_, doc)) in rows.iter().enumerate() {
                let mut obj = match items {
                    None => doc.as_object().cloned().unwrap_or_default(),
                    Some(items) => {
                        let mut m = Map::new();
                        for it in items {
                            match it {
                                Item::All => m.extend(doc.as_object().cloned().unwrap_or_default()),
                                Item::Field(p) => {
                                    if let Some(v) = get_path(doc, p) {
                                        insert_path(&mut m, p, v.clone());
                                    }
                                }
                                Item::Rel(_) => {}
                            }
                        }
                        m
                    }
                };
                for (name, vals) in &attached {
                    obj.insert(name.clone(), vals[i].clone());
                }
                out.push(Value::Object(obj));
            }
            Ok(out)
        })
    }

    /// The value of one relation for every parent row, in parent order.
    async fn attach(
        &self,
        rel: &Rel,
        parents: &[Row],
        budget: &mut usize,
        depth: usize,
    ) -> Result<Vec<Value>, QueryError> {
        let lv = &rel.level;
        match (&rel.def.by, &rel.def.id) {
            // has-many: one query for all parents' children
            (Some(by), _) => {
                let keys: Vec<String> = parents.iter().map(|(id, _)| id.clone()).collect();
                if keys.is_empty() {
                    return Ok(vec![]);
                }
                let (children, _) = self.fetch(lv, Some((by, keys)), budget).await?;
                let per_parent = lv
                    .limit
                    .unwrap_or(self.limits.max_limit)
                    .clamp(1, self.limits.max_limit);
                let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
                for (i, (_, d)) in children.iter().enumerate() {
                    if let Some(k) = get_path(d, by).and_then(key_text) {
                        let g = groups.entry(k).or_default();
                        if g.len() < per_parent {
                            g.push(i);
                        }
                    }
                }
                // nested relations are resolved once, over the children that survived the per-parent limit
                let keep: Vec<usize> = groups
                    .values()
                    .flatten()
                    .copied()
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                let mut slot: HashMap<usize, usize> = HashMap::new();
                let kept_rows: Vec<Row> = keep
                    .iter()
                    .enumerate()
                    .map(|(n, &i)| {
                        slot.insert(i, n);
                        children[i].clone()
                    })
                    .collect();
                let resolved = self.resolve(lv, kept_rows, budget, depth).await?;
                Ok(parents
                    .iter()
                    .map(|(pid, _)| {
                        Value::Array(
                            groups
                                .get(pid)
                                .map(|g| g.iter().map(|i| resolved[slot[i]].clone()).collect())
                                .unwrap_or_default(),
                        )
                    })
                    .collect())
            }
            // belongs-to: one batched lookup by id
            (None, Some(fk)) => {
                self.readable(&lv.from)?;
                let mut wanted: Vec<String> = parents
                    .iter()
                    .filter_map(|(_, d)| get_path(d, fk).and_then(key_text))
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                wanted.sort();
                let mut found = if wanted.is_empty() {
                    vec![]
                } else {
                    self.store.get_many(&lv.from, &wanted).await?
                };
                self.charge(budget, found.len())?;
                if let Some(w) = &lv.where_ {
                    found.retain(|(_, d)| check_require(Some(d), w).is_ok());
                }
                let resolved = self.resolve(lv, found.clone(), budget, depth).await?;
                let by_id: HashMap<&str, &Value> = found
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .zip(resolved.iter())
                    .collect();
                Ok(parents
                    .iter()
                    .map(|(_, d)| {
                        get_path(d, fk)
                            .and_then(key_text)
                            .and_then(|k| by_id.get(k.as_str()).map(|v| (*v).clone()))
                            .unwrap_or(Value::Null)
                    })
                    .collect())
            }
            (None, None) => Err(QueryError::Invalid(
                "relation has neither `by` nor `id`".into(),
            )),
        }
    }
}

#[allow(dead_code)]
fn _assert_anyhow(_: AnyResult<()>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rels() -> Relations {
        serde_json::from_value(json!({ "orders": { "items": {"from": "orderItems", "by": "orderId"}, "customer": {"from": "customers", "id": "customerId"} } })).unwrap()
    }

    fn ok(v: Value) -> Query {
        parse_query(&v, &rels(), 4).unwrap()
    }

    fn err(v: Value) -> String {
        parse_query(&v, &rels(), 4).unwrap_err()
    }

    #[test]
    fn parses_the_documented_shapes() {
        let q = ok(
            json!({"from": "orders", "where": {"status": {"in": ["open"]}}, "orderBy": [{"createdAt": "desc"}, "-total", "name"],
            "limit": 5, "offset": 2, "total": true,
            "select": ["id", "owner.name", {"items": {"from": "orderItems", "by": "orderId", "select": ["sku"], "limit": 3}}, {"customer": {"from": "customers", "id": "customerId"}}]}),
        );
        assert_eq!(
            q.order_by,
            vec![
                ("createdAt".into(), true),
                ("total".into(), true),
                ("name".into(), false)
            ]
        );
        assert_eq!((q.limit, q.offset, q.total), (Some(5), 2, true));
        let items = q.select.unwrap();
        assert!(
            matches!(&items[2], Item::Rel(r) if r.name == "items" && r.def.by.as_deref() == Some("orderId") && r.level.limit == Some(3))
        );
        assert!(matches!(&items[3], Item::Rel(r) if r.def.id.as_deref() == Some("customerId")));
        // named relations, with overrides
        let q = ok(
            json!({"from": "orders", "select": [{"items": "items"}, {"c": {"rel": "customer", "select": ["name"]}}]}),
        );
        let items = q.select.unwrap();
        assert!(matches!(&items[0], Item::Rel(r) if r.level.from == "orderItems"));
        assert!(
            matches!(&items[1], Item::Rel(r) if r.name == "c" && r.def.from == "customers" && r.level.select.is_some())
        );
        assert_eq!(
            ok(json!({"from": "orders", "id": 42})).id.as_deref(),
            Some("42")
        );
        assert_eq!(
            ok(json!({"from": "orders", "orderBy": "-total"})).order_by,
            vec![("total".into(), true)]
        );
    }

    #[test]
    fn rejects_malformed_queries_with_specific_messages() {
        assert!(err(json!([])).contains("JSON object"));
        assert!(err(json!({})).contains("needs `from`"));
        assert!(err(json!({"from": "a b"})).contains("collection"));
        assert!(err(json!({"from": "o", "frm": 1})).contains("unknown key `frm`"));
        assert!(err(json!({"from": "o", "where": [1]})).contains("where must be an object"));
        assert!(
            err(json!({"from": "o", "where": {"a": {"between": [1, 2]}}}))
                .contains("unknown operator")
        );
        assert!(err(json!({"from": "o", "orderBy": [{"a": "up"}]})).contains("asc"));
        assert!(err(json!({"from": "o", "orderBy": ["a b"]})).contains("valid field"));
        assert!(err(json!({"from": "o", "limit": -1})).contains("non-negative"));
        assert!(err(json!({"from": "o", "id": 1, "limit": 3})).contains("one document"));
        assert!(err(json!({"from": "o", "select": "id"})).contains("array"));
        assert!(
            err(json!({"from": "o", "select": [{"a": 1}]})).contains("relation name or an object")
        );
        assert!(
            err(json!({"from": "o", "select": [{"a": {"from": "x"}}]})).contains("exactly one of")
        );
        assert!(
            err(json!({"from": "o", "select": [{"a": {"from": "x", "by": "k", "id": "j"}}]}))
                .contains("exactly one of")
        );
        assert!(err(json!({"from": "orders", "select": [{"a": "nope"}]}))
            .contains("no relation `nope`"));
        assert!(
            err(json!({"from": "orders", "select": [{"a": {"rel": "items", "from": "x"}}]}))
                .contains("cannot be combined")
        );
        assert!(err(
            json!({"from": "orders", "select": [{"a": {"from": "x", "by": "k", "bogus": 1}}]})
        )
        .contains("unknown key `bogus`"));
        // depth
        let mut deep = json!({"from": "c", "by": "k"});
        for _ in 0..6 {
            deep = json!({"from": "c", "by": "k", "select": [{"n": deep}]});
        }
        assert!(err(json!({"from": "o", "select": [{"n": deep}]})).contains("deeper than 4"));
    }

    #[test]
    fn json_ordering_is_total_and_stable() {
        use std::cmp::Ordering::*;
        let v = |x: Value| x;
        assert_eq!(cmp_values(None, Some(&v(json!(1)))), Less, "missing first");
        assert_eq!(
            cmp_values(Some(&json!(null)), None),
            Equal,
            "null and missing tie"
        );
        assert_eq!(cmp_values(Some(&json!(true)), Some(&json!(0))), Less);
        assert_eq!(
            cmp_values(Some(&json!(2)), Some(&json!(10))),
            Less,
            "numbers numerically"
        );
        assert_eq!(cmp_values(Some(&json!("b")), Some(&json!("a"))), Greater);
        assert_eq!(
            cmp_values(Some(&json!(99)), Some(&json!("a"))),
            Less,
            "numbers before strings"
        );
        let mut rows: Vec<Row> = vec![
            ("1".into(), json!({"k": 2, "n": "x"})),
            ("2".into(), json!({"k": 1, "n": "y"})),
            ("3".into(), json!({"k": 2, "n": "a"})),
            ("4".into(), json!({"n": "z"})),
        ];
        sort_rows(&mut rows, &[("k".into(), true), ("n".into(), false)]);
        let ids: Vec<&str> = rows.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(
            ids,
            ["3", "1", "2", "4"],
            "k desc (missing last), then n asc"
        );
    }

    #[test]
    fn only_simple_equality_is_pushed_to_the_database() {
        let w = json!({"a": "x", "b": 5, "c": true, "d": {"eq": "y"}, "e": 1.5, "f": {"gte": 3}, "g": null, "h": {"eq": null}, "i": {"eq": 1, "ne": 2}});
        let mut p = pushdown(w.as_object().unwrap());
        p.sort();
        assert_eq!(
            p,
            vec![
                ("a".into(), "x".into()),
                ("b".into(), "5".into()),
                ("c".into(), "true".into()),
                ("d".into(), "y".into())
            ]
        );
    }

    #[test]
    fn projection_builds_nested_output_from_dotted_paths() {
        let mut m = Map::new();
        insert_path(&mut m, "owner.name", json!("ann"));
        insert_path(&mut m, "owner.age", json!(3));
        insert_path(&mut m, "id", json!(1));
        assert_eq!(
            Value::Object(m),
            json!({"owner": {"name": "ann", "age": 3}, "id": 1})
        );
        assert_eq!(key_text(&json!(7)).as_deref(), Some("7"));
        assert_eq!(
            key_text(&json!("7")).as_deref(),
            Some("7"),
            "1 and \"1\" join"
        );
        assert_eq!(key_text(&json!({})), None);
    }

    #[test]
    fn relation_definitions_are_validated() {
        assert!(validate_relations(&rels()).is_ok());
        let bad = |v: Value| validate_relations(&serde_json::from_value(v).unwrap()).unwrap_err();
        assert!(bad(json!({"orders": {"x": {"from": "c"}}})).contains("exactly one of"));
        assert!(bad(json!({"orders": {"x": {"from": "a b", "by": "k"}}})).contains("target"));
        assert!(bad(json!({"orders": {"a.b": {"from": "c", "by": "k"}}})).contains("relation name"));
        assert!(bad(json!({"bad coll": {"x": {"from": "c", "by": "k"}}})).contains("collection"));
        assert!(
            serde_json::from_value::<RelationDef>(json!({"from": "c", "by": "k", "typo": 1}))
                .is_err()
        );
    }
}
