//! `/db`: inspect, seed and reset the data behind the API (for a management UI and test setup).

use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::{
    db::{crud::IdKind, valid_collection, DocStore, ListQuery},
    error::ApiError,
    state::AppState,
};

type Resp = Result<Response, Response>;

fn err(status: u16, code: &str, msg: impl Into<String>) -> Response {
    ApiError::new(status, code, msg).into_response()
}

fn store(st: &AppState) -> Result<Arc<dyn DocStore>, Response> {
    st.db.clone().ok_or_else(|| {
        err(
            409,
            "database_disabled",
            "the database is disabled; set `database.enabled: true` and restart",
        )
    })
}

fn coll(name: &str) -> Result<(), Response> {
    valid_collection(name).then_some(()).ok_or_else(|| {
        err(
            400,
            "bad_name",
            "collection must be 1-64 characters of [A-Za-z0-9_-]",
        )
    })
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!(error = %format!("{e:#}"), "database error in control plane");
    err(500, "database_error", "the database request failed")
}

pub(super) async fn collections(State(st): State<Arc<AppState>>) -> Resp {
    let db = store(&st)?;
    let list = db.collections().await.map_err(internal)?;
    Ok(Json(json!({ "collections": list.into_iter().map(|(name, count)| json!({"name": name, "count": count})).collect::<Vec<_>>() })).into_response())
}

pub(super) async fn docs(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Resp {
    let db = store(&st)?;
    coll(&name)?;
    let mut lq = ListQuery {
        limit: 100,
        ..Default::default()
    };
    for (k, v) in &q {
        match k.as_str() {
            "limit" => {
                lq.limit = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=1000).contains(n))
                    .ok_or_else(|| err(400, "bad_query", "limit must be 1..=1000"))?
            }
            "offset" => {
                lq.offset = v
                    .parse()
                    .map_err(|_| err(400, "bad_query", "offset must be a non-negative integer"))?
            }
            "sort" => {
                lq.sort = Some(
                    v.strip_prefix('-')
                        .map(|f| (f.to_string(), true))
                        .unwrap_or((v.clone(), false)),
                )
            }
            // anything else is an equality filter on that field
            _ => lq.filters.push((k.clone(), v.clone())),
        }
    }
    let total = db
        .count(&name, &lq.filters)
        .await
        .map_err(|e| err(400, "bad_query", format!("{e:#}")))?;
    let docs = db
        .list(&name, &lq)
        .await
        .map_err(|e| err(400, "bad_query", format!("{e:#}")))?;
    Ok(Json(json!({ "collection": name, "total": total, "docs": docs })).into_response())
}

/// Seed documents (one object or an array). Ids follow the API: the CRUD plan for the collection
/// says which field holds the id and whether it is an integer sequence or a uuid.
pub(super) async fn seed(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    body: Bytes,
) -> Resp {
    let db = store(&st)?;
    coll(&name)?;
    let value: Value =
        serde_json::from_slice(&body).map_err(|e| err(400, "bad_json", e.to_string()))?;
    let items = match value {
        Value::Array(a) => a,
        o @ Value::Object(_) => vec![o],
        _ => {
            return Err(err(
                400,
                "bad_request",
                "body must be an object or an array of objects",
            ))
        }
    };
    let rt = st.runtime.load();
    let (field, kind) = rt
        .table
        .routes()
        .iter()
        .filter_map(|r| r.crud.as_ref())
        .find(|p| p.collection == name)
        .map(|p| (p.id_field.clone(), p.id_kind))
        .unwrap_or_else(|| ("id".to_string(), IdKind::String));

    let (mut inserted, mut skipped, mut ids) = (0u64, 0u64, Vec::new());
    for item in items {
        let Value::Object(mut doc) = item else {
            return Err(err(
                400,
                "bad_request",
                "every document must be a JSON object",
            ));
        };
        let id = match doc.get(&field).filter(|v| !v.is_null()) {
            Some(Value::Number(n)) if kind == IdKind::Integer && n.is_i64() => {
                let id = n.as_i64().unwrap_or_default();
                db.bump_sequence(&name, id).await.map_err(internal)?;
                id.to_string()
            }
            Some(Value::String(s)) if kind == IdKind::String && !s.is_empty() => s.clone(),
            Some(_) => {
                return Err(err(
                    400,
                    "invalid_id",
                    format!(
                        "`{field}` must be {}",
                        if kind == IdKind::Integer {
                            "an integer"
                        } else {
                            "a non-empty string"
                        }
                    ),
                ))
            }
            None => match kind {
                IdKind::Integer => {
                    let id = db.next_sequence(&name).await.map_err(internal)?;
                    doc.insert(field.clone(), json!(id));
                    id.to_string()
                }
                IdKind::String => {
                    let id = uuid::Uuid::new_v4().to_string();
                    doc.insert(field.clone(), json!(id));
                    id
                }
            },
        };
        if db
            .insert(&name, &id, &Value::Object(doc))
            .await
            .map_err(internal)?
        {
            inserted += 1;
            ids.push(id);
        } else {
            skipped += 1;
        }
    }
    let mut resp = Json(json!({ "collection": name, "inserted": inserted, "skipped_existing": skipped, "ids": ids })).into_response();
    *resp.status_mut() = StatusCode::CREATED;
    Ok(resp)
}

/// Run the read language with unrestricted access (an operator tool: grants do not apply here).
pub(super) async fn query(State(st): State<Arc<AppState>>, body: Bytes) -> Resp {
    use crate::db::query::{http_error, parse_query, Engine, Limits};
    let db = store(&st)?;
    let v: Value =
        serde_json::from_slice(&body).map_err(|e| err(400, "bad_json", e.to_string()))?;
    let rt = st.runtime.load_full();
    let limits = Limits::default();
    let q = parse_query(&v, &rt.relations, limits.max_depth)
        .map_err(|m| err(400, "invalid_query", m))?;
    let allow_all = |_: &str| Ok(());
    let out = Engine {
        store: db.as_ref(),
        limits,
        can_read: &allow_all,
    }
    .run(&q)
    .await
    .map_err(|e| {
        let c = http_error(&e);
        err(c.status, &c.code, c.message)
    })?;
    Ok(Json(out).into_response())
}

pub(super) async fn clear(State(st): State<Arc<AppState>>, Path(name): Path<String>) -> Resp {
    let db = store(&st)?;
    coll(&name)?;
    let n = db.clear(&name).await.map_err(internal)?;
    Ok(Json(json!({ "collection": name, "deleted": n })).into_response())
}
