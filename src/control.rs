//! The control plane: a separate listener for a management frontend.
//! Every route is documented in `openapi/control-plane.yaml`; a test fails on drift.

use std::{
    collections::{BTreeSet, HashMap},
    net::SocketAddr,
    sync::Arc,
};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put, MethodRouter},
    Json, Router,
};
use serde_json::{json, Value};

mod builder;
mod db;
mod scripts;

use crate::{
    auth::ct_eq,
    config::RatePolicy,
    error::ApiError,
    state::{AppState, ApplyError, ApplyOpts, ApplyReport},
};

pub const OPENAPI_YAML: &str = include_str!("../openapi/control-plane.yaml");
const BASE: &str = "/api/v1";
/// Reachable without a token (probes and the contract itself).
const PUBLIC: [&str; 3] = ["/api/v1/health", "/api/v1/ready", "/api/v1/openapi.yaml"];

type Table = Vec<(
    &'static str,
    &'static [&'static str],
    MethodRouter<Arc<AppState>>,
)>;

/// Single source of truth for what this API serves. Methods are declared next to the
/// handlers so a test can compare them with `openapi/control-plane.yaml`.
fn table() -> Table {
    vec![
        ("/health", &["get"], get(health)),
        ("/ready", &["get"], get(ready)),
        ("/status", &["get"], get(status)),
        ("/config", &["get", "put"], get(get_config).put(put_config)),
        ("/reload", &["post"], post(reload)),
        ("/routes", &["get"], get(routes)),
        ("/hooks", &["get"], get(hooks)),
        ("/metrics", &["get"], get(metrics)),
        ("/openapi.yaml", &["get"], get(openapi)),
        ("/db/query", &["post"], post(db::query)),
        ("/db/collections", &["get"], get(db::collections)),
        ("/db/collections/:name", &["delete"], delete(db::clear)),
        (
            "/db/collections/:name/docs",
            &["get", "post"],
            get(db::docs).post(db::seed),
        ),
        ("/builder", &["get"], get(builder::status)),
        ("/builder/typings", &["get"], get(builder::typings)),
        (
            "/builder/templates/:language",
            &["get"],
            get(builder::template),
        ),
        ("/scripts", &["get"], get(scripts::list)),
        (
            "/scripts/:name/source",
            &["post"],
            post(builder::build_source),
        ),
        (
            "/scripts/:name/versions/:version/source",
            &["get"],
            get(builder::get_source),
        ),
        ("/scripts/:name", &["get"], get(scripts::get_script)),
        (
            "/scripts/:name/versions",
            &["post"],
            post(scripts::upload_version),
        ),
        (
            "/scripts/:name/versions/:version",
            &["delete"],
            delete(scripts::delete_version),
        ),
        (
            "/scripts/:name/versions/:version/test",
            &["post"],
            post(scripts::test_version),
        ),
        (
            "/scripts/:name/aliases/:alias",
            &["put", "delete"],
            put(scripts::put_alias).delete(scripts::delete_alias),
        ),
    ]
}

/// `(method, path)` pairs actually routed, e.g. `("get", "/status")`.
pub fn routes_served() -> BTreeSet<(String, String)> {
    table()
        .iter()
        .flat_map(|(path, methods, _)| {
            methods
                .iter()
                .map(move |m| (m.to_string(), openapi_path(path)))
        })
        .collect()
}

/// axum `:name` → OpenAPI `{name}`.
fn openapi_path(p: &str) -> String {
    p.split('/')
        .map(|seg| {
            seg.strip_prefix(':')
                .map_or(seg.to_string(), |n| format!("{{{n}}}"))
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn router(state: Arc<AppState>) -> Router {
    let upload_limit = state.config().wasm.max_upload_mb.max(1) * 1024 * 1024;
    let api = table()
        .into_iter()
        .fold(Router::new(), |r, (path, _, handler)| {
            r.route(path, handler)
        });
    Router::new()
        .nest(
            BASE,
            api.layer(axum::extract::DefaultBodyLimit::max(upload_limit)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

async fn require_token(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if PUBLIC.contains(&req.uri().path()) {
        return next.run(req).await;
    }
    // Brute-force guard: 10 failures/min per IP. Checked *before* comparing, so a blocked
    // IP learns nothing from guessing (even a correct token gets 429 while blocked).
    let policy = RatePolicy {
        requests: 10,
        per_seconds: 60,
        burst: None,
        key: "ip".into(),
    };
    let key = format!("control-auth|{}", peer.ip());
    if st.limiter.is_blocked(&key, &policy) {
        return ApiError::new(
            429,
            "too_many_failures",
            "too many failed attempts; slow down",
        )
        .with_header("retry-after", "60")
        .into_response();
    }
    let token = st.config().control.token.clone();
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.get(..7)
                .filter(|p| p.eq_ignore_ascii_case("bearer "))
                .map(|_| &v[7..])
        })
        .map(str::trim);
    if presented.is_some_and(|p| !token.is_empty() && ct_eq(p.as_bytes(), token.as_bytes())) {
        return next.run(req).await;
    }
    st.limiter.check(&key, &policy);
    st.metrics.inc_auth_failure();
    ApiError::new(401, "unauthorized", "missing or invalid control token")
        .with_header("www-authenticate", "Bearer")
        .into_response()
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn ready(State(st): State<Arc<AppState>>) -> Response {
    let database = match &st.db {
        None => "disabled",
        Some(d) => {
            if d.ping().await.is_ok() {
                "ok"
            } else {
                "down"
            }
        }
    };
    let ok = database != "down";
    let mut resp = Json(json!({ "status": if ok { "ready" } else { "degraded" }, "config_version": st.version(), "database": database })).into_response();
    if !ok {
        *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    }
    resp
}

async fn status(State(st): State<Arc<AppState>>) -> Json<Value> {
    let rt = st.runtime.load();
    let cfg = &rt.config;
    let listener = |name: &str, enabled_tls: bool, mtls: bool| {
        st.bound(name)
            .map(|a| json!({ "addr": a.to_string(), "tls": enabled_tls, "mtls": mtls }))
    };
    Json(json!({
        "app_name": cfg.app_name,
        "config_version": st.version(),
        "uptime_seconds": st.started.elapsed().as_secs(),
        "spec": { "title": rt.spec_title, "version": rt.spec_version, "path": cfg.openapi.spec_path },
        "routes": rt.table.routes().len(),
        "database": { "enabled": cfg.database.enabled, "type": format!("{:?}", cfg.database.db_type).to_lowercase() },
        "wasm": { "enabled": cfg.wasm.enabled },
        "pending_process_restart": st.pending_process_restart(),
        "listeners": {
            "data_plane": listener("data_plane", cfg.server.tls.enabled, cfg.server.tls.client_ca_path.is_some()),
            "control_plane": listener("control_plane", cfg.control.tls.enabled, cfg.control.tls.client_ca_path.is_some()),
        },
    }))
}

async fn get_config(State(st): State<Arc<AppState>>) -> Response {
    let version = st.version();
    let body = json!({ "version": version, "config": st.config().redacted() });
    let mut resp = Json(body).into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("\"{version}\"")) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp
}

async fn put_config(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let incoming: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return ApiError::new(400, "bad_json", format!("body is not valid JSON: {e}"))
                .into_response()
        }
    };
    let opts = ApplyOpts {
        dry_run: truthy(&q, "dry_run"),
        if_match: if_match(&headers),
    };
    respond(st.apply_json(incoming, opts).await)
}

async fn reload(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    respond(
        st.reload_from_disk(ApplyOpts {
            dry_run: truthy(&q, "dry_run"),
            if_match: None,
        })
        .await,
    )
}

fn respond(r: Result<ApplyReport, ApplyError>) -> Response {
    match r {
        Ok(report) => Json(report).into_response(),
        Err(e @ ApplyError::Invalid(_)) => {
            ApiError::new(422, "invalid_config", e.to_string()).into_response()
        }
        Err(ApplyError::Conflict { current }) => ApiError::new(
            412,
            "version_conflict",
            format!("config is at version {current}; re-fetch and retry"),
        )
        .with_details(json!({ "current_version": current }))
        .into_response(),
        Err(e @ ApplyError::Persist(_)) => {
            ApiError::new(500, "persist_failed", e.to_string()).into_response()
        }
    }
}

async fn routes(State(st): State<Arc<AppState>>) -> Json<Value> {
    let rt = st.runtime.load();
    let list: Vec<Value> = rt
        .table
        .routes()
        .iter()
        .map(|r| {
            let steps: serde_json::Map<String, Value> = r
                .steps
                .iter()
                .map(|(step, p)| {
                    (
                        step.as_str().to_string(),
                        json!({
                            "before": p.before.iter().map(|h| hook_json(h.as_ref())).collect::<Vec<_>>(),
                            "replace": p.replace.as_ref().map(|h| hook_json(h.as_ref())),
                            "after": p.after.iter().map(|h| hook_json(h.as_ref())).collect::<Vec<_>>(),
                        }),
                    )
                })
                .collect();
            json!({
                "id": r.id,
                "operation_id": r.operation_id,
                "tags": r.tags,
                "protected": r.protected,
                "required_roles": r.required_roles,
                "data_rules": r.data.summary(),
                "crud": r.crud.as_ref().map(|c| json!({ "operation": c.op, "collection": c.collection })),
                "rate_limit": r.rate.as_ref().map(|c| &c.policy),
                "response_status": r.response.status,
                "steps": steps,
            })
        })
        .collect();
    Json(json!({ "routes": list }))
}

async fn hooks(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "hooks": st.registry.names() }))
}

async fn metrics(State(st): State<Arc<AppState>>) -> Response {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        st.metrics.render(st.version()),
    )
        .into_response()
}

async fn openapi() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/yaml")],
        OPENAPI_YAML,
    )
        .into_response()
}

fn truthy(q: &HashMap<String, String>, k: &str) -> bool {
    q.get(k)
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

fn if_match(h: &HeaderMap) -> Option<u64> {
    h.get(header::IF_MATCH)?
        .to_str()
        .ok()?
        .trim()
        .trim_matches('"')
        .parse()
        .ok()
}

fn hook_json(h: &dyn crate::hooks::Hook) -> Value {
    json!({ "ref": h.name(), "pinned": h.pinned() })
}
