//! `/scripts`: upload, version, alias and *test* WebAssembly hooks.

use std::{collections::HashMap, net::IpAddr, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{Method, Request},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::{
    auth::Identity,
    config::Config,
    error::ApiError,
    pipeline::{Outcome, RequestContext, Step},
    routes::Lookup,
    state::AppState,
    wasm::{store::parse_ref, ScriptStore, WasmRuntime, WasmSupport},
};

type Resp = Result<Response, Response>;

pub(super) fn err(status: u16, code: &str, msg: impl Into<String>) -> Response {
    ApiError::new(status, code, msg).into_response()
}

pub(super) fn wasm(
    st: &AppState,
) -> Result<(WasmSupport, Arc<ScriptStore>, Arc<WasmRuntime>), Response> {
    match (&st.scripts, &st.wasm) {
        (Some(s), Some(r)) => Ok((
            WasmSupport {
                runtime: r.clone(),
                store: s.clone(),
            },
            s.clone(),
            r.clone(),
        )),
        _ => Err(err(
            409,
            "wasm_disabled",
            "the WASM runtime is disabled; set `wasm.enabled: true` and restart",
        )),
    }
}

/// A hook slot in the config that references a script.
struct Usage {
    route: String,
    step: &'static str,
    slot: &'static str,
    reference: String,
}

fn usages(cfg: &Config, script: &str) -> Vec<Usage> {
    let mut out = Vec::new();
    for (route, rc) in &cfg.routes {
        for (step, sh) in &rc.steps {
            let mut push = |slot: &'static str, r: &String| {
                if parse_ref(r).is_some_and(|(n, _)| n == script) {
                    out.push(Usage {
                        route: route.clone(),
                        step: step.as_str(),
                        slot,
                        reference: r.clone(),
                    });
                }
            };
            sh.before.iter().for_each(|r| push("before", r));
            sh.replace.iter().for_each(|r| push("replace", r));
            sh.after.iter().for_each(|r| push("after", r));
        }
    }
    out
}

fn usage_json(u: &[Usage]) -> Value {
    Value::Array(
        u.iter()
            .map(|u| json!({"route": u.route, "step": u.step, "slot": u.slot, "ref": u.reference}))
            .collect(),
    )
}

pub(super) async fn list(State(st): State<Arc<AppState>>) -> Resp {
    let (_, store, _) = wasm(&st)?;
    let cfg = st.config();
    let scripts: Vec<Value> = store
        .list()
        .into_iter()
        .map(|(name, s)| {
            let latest = s.versions.last();
            json!({
                "name": name, "id": s.id, "created_at": s.created_at,
                "versions": s.versions.len(),
                "latest_version": latest.map(|v| v.version),
                "language": latest.map(|v| v.language.clone()),
                "aliases": s.aliases,
                "used_by": usages(&cfg, &name).len(),
            })
        })
        .collect();
    Ok(Json(json!({ "scripts": scripts })).into_response())
}

pub(super) async fn get_script(State(st): State<Arc<AppState>>, Path(name): Path<String>) -> Resp {
    let (_, store, _) = wasm(&st)?;
    let s = store
        .get(&name)
        .ok_or_else(|| err(404, "not_found", format!("no script `{name}`")))?;
    let used = usages(&st.config(), &name);
    Ok(Json(json!({ "name": name, "script": s, "used_by": usage_json(&used) })).into_response())
}

/// Validate a component against the host contract, compile it ahead of time and store it.
/// Shared by direct uploads and builds from source. CPU-heavy, so it runs off the async threads.
pub(super) async fn store_component(
    store: Arc<ScriptStore>,
    rt: Arc<WasmRuntime>,
    bytes: Bytes,
) -> Result<String, Response> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let digest = store.put_artifact(&bytes)?;
        let known = store
            .list()
            .values()
            .flat_map(|s| s.versions.iter())
            .any(|v| v.digest == digest);
        if !(known && store.cwasm_path(&digest)?.exists()) {
            if let Err(e) = rt.compile_and_verify(&digest, &bytes) {
                if !known {
                    let _ = std::fs::remove_file(store.wasm_path(&digest)?);
                }
                return Err(e);
            }
        }
        Ok(digest)
    })
    .await
    .map_err(|e| err(500, "internal", e.to_string()))?
    .map_err(|e| err(422, "invalid_component", format!("{e:#}")))
}

pub(super) async fn upload_version(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Resp {
    let (_, store, rt) = wasm(&st)?;
    if !crate::wasm::store::valid_name(&name) {
        return Err(err(
            400,
            "bad_name",
            "script name must match [a-z0-9][a-z0-9_-]{0,62}",
        ));
    }
    if body.len() < 8 || &body[..4] != b"\0asm" {
        return Err(err(
            400,
            "not_wasm",
            "body must be a WebAssembly component (raw bytes, Content-Type: application/wasm)",
        ));
    }
    let language = q
        .get("language")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let note = q.get("note").cloned().unwrap_or_default();
    let size = body.len() as u64;
    let digest = store_component(store.clone(), rt.clone(), body).await?;

    let (v, created) = store
        .add_version(&name, &digest, size, &language, &note, None)
        .map_err(|e| err(400, "bad_request", format!("{e:#}")))?;
    let script = store.get(&name).expect("just written");
    let mut resp = Json(json!({
        "name": name, "script_id": script.id, "version": v.version, "digest": v.digest,
        "size": v.size, "language": v.language, "created": created,
        "reference": format!("wasm:{name}@{}", v.version),
    }))
    .into_response();
    *resp.status_mut() = if created {
        axum::http::StatusCode::CREATED
    } else {
        axum::http::StatusCode::OK
    };
    Ok(resp)
}

pub(super) async fn delete_version(
    State(st): State<Arc<AppState>>,
    Path((name, version)): Path<(String, u32)>,
) -> Resp {
    let (support, store, rt) = wasm(&st)?;
    let cfg = st.config();
    for u in usages(&cfg, &name) {
        let (n, r) = parse_ref(&u.reference).expect("filtered by usages()");
        if support
            .store
            .resolve(n, r)
            .is_ok_and(|res| res.version == version)
        {
            return Err(err(
                409,
                "in_use",
                format!(
                    "version {version} is used by `{}` ({} {} via `{}`); rebind the route first",
                    u.route, u.step, u.slot, u.reference
                ),
            ));
        }
    }
    let removed = store
        .delete_version(&name, version)
        .map_err(|e| err(409, "cannot_delete", format!("{e:#}")))?;
    if !store
        .list()
        .values()
        .flat_map(|s| &s.versions)
        .any(|v| v.digest == removed.digest)
    {
        rt.evict(&removed.digest);
    }
    Ok(Json(json!({ "deleted": removed })).into_response())
}

pub(super) async fn put_alias(
    State(st): State<Arc<AppState>>,
    Path((name, alias)): Path<(String, String)>,
    body: Bytes,
) -> Resp {
    let (_, store, _) = wasm(&st)?;
    let version = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("version")?.as_u64())
        .ok_or_else(|| err(400, "bad_request", r#"body must be {"version": <number>}"#))?
        as u32;
    let previous = store
        .get(&name)
        .and_then(|s| s.aliases.get(&alias).copied());
    store
        .set_alias(&name, &alias, version)
        .map_err(|e| err(422, "bad_alias", format!("{e:#}")))?;

    // If any route follows this alias, re-resolve now; if the rebuild fails, put the alias back.
    let followed = usages(&st.config(), &name)
        .iter()
        .any(|u| parse_ref(&u.reference).is_some_and(|(_, r)| r == alias));
    let mut applied = Value::Null;
    if followed {
        match st.rebuild_runtime().await {
            Ok(rep) => applied = json!({ "config_version": rep.version }),
            Err(e) => {
                let _ = match previous {
                    Some(p) => store.set_alias(&name, &alias, p),
                    None => store.delete_alias(&name, &alias),
                };
                return Err(err(422, "rebuild_failed", format!("alias reverted: {e}")));
            }
        }
    }
    Ok(Json(json!({ "name": name, "alias": alias, "version": version, "previous": previous, "applied": applied })).into_response())
}

pub(super) async fn delete_alias(
    State(st): State<Arc<AppState>>,
    Path((name, alias)): Path<(String, String)>,
) -> Resp {
    let (_, store, _) = wasm(&st)?;
    if let Some(u) = usages(&st.config(), &name)
        .into_iter()
        .find(|u| parse_ref(&u.reference).is_some_and(|(_, r)| r == alias))
    {
        return Err(err(
            409,
            "in_use",
            format!("alias `{alias}` is used by `{}` ({})", u.route, u.step),
        ));
    }
    store
        .delete_alias(&name, &alias)
        .map_err(|e| err(404, "not_found", format!("{e:#}")))?;
    Ok(Json(json!({ "deleted": alias })).into_response())
}

/// Run one hook version against a fixture, in the real sandbox, with the real contract checks,
/// without binding it to anything. This is the "test before you deploy" endpoint.
pub(super) async fn test_version(
    State(st): State<Arc<AppState>>,
    Path((name, version)): Path<(String, u32)>,
    body: Bytes,
) -> Resp {
    let (support, _, _) = wasm(&st)?;
    let t: Value =
        serde_json::from_slice(&body).map_err(|e| err(400, "bad_json", e.to_string()))?;
    let step: Step = serde_json::from_value(t.get("step").cloned().unwrap_or(json!("pre_process")))
        .map_err(|_| {
            err(
                400,
                "bad_step",
                "step must be one of the pipeline steps, e.g. pre_process",
            )
        })?;

    let rq = t.get("request").ok_or_else(|| {
        err(
            400,
            "bad_request",
            "`request` {method, path, headers?, body?} is required",
        )
    })?;
    let method = Method::from_bytes(
        rq.get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_uppercase()
            .as_bytes(),
    )
    .map_err(|_| err(400, "bad_request", "invalid method"))?;
    let target = rq
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| err(400, "bad_request", "`request.path` is required"))?;
    let path_only = target.split('?').next().unwrap_or(target);

    let rt = st.runtime.load_full();
    let (route, params) = match rt.table.lookup(&method, path_only) {
        Lookup::Found(r, p) => (r, p),
        _ => return Err(err(422, "no_route", format!("no served route matches {method} {path_only}; hooks are tested in the context of a route"))),
    };

    let mut b = Request::builder().method(method).uri(target);
    for (k, v) in rq
        .get("headers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if let Some(v) = v.as_str() {
            b = b.header(k.as_str(), v);
        }
    }
    let (parts, _) = b
        .body(())
        .map_err(|e| err(400, "bad_request", e.to_string()))?
        .into_parts();
    let raw = rq
        .get("body")
        .filter(|b| !b.is_null())
        .map(|b| Bytes::from(b.to_string()))
        .unwrap_or_default();
    let mut ctx = RequestContext::new(
        route.clone(),
        &parts,
        params,
        raw,
        IpAddr::from([127, 0, 0, 1]),
        format!("test-{}", uuid::Uuid::new_v4().simple()),
    );
    if let Some(Value::Object(s)) = t.get("state") {
        ctx.state = s.clone();
    }
    if let Some(id) = t.get("identity").filter(|i| i.is_object()) {
        ctx.identity = Some(Identity {
            subject: id
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("test")
                .to_string(),
            roles: id
                .get("roles")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    if let Some(r) = t.get("response") {
        ctx.response.status = r
            .get("status")
            .and_then(Value::as_u64)
            .map(|s| s as u16)
            .unwrap_or(200);
        ctx.response.body = r.get("body").filter(|b| !b.is_null()).cloned();
        for (k, v) in r
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if let Some(v) = v.as_str() {
                ctx.response.headers.push((k.clone(), v.to_string()));
            }
        }
    } else {
        // default: what the built-in `execute` would have produced for this route
        ctx.response.status = route.response.status;
        ctx.response.body = route.response.mock.clone();
    }

    ctx.relations = rt.relations.clone();
    // Database during a test: read-only by default so testing can never change real data.
    ctx.db = match (
        t.get("db").and_then(Value::as_str).unwrap_or("readonly"),
        rt.db.clone(),
    ) {
        (_, None) | ("none", _) => None,
        ("live", Some(d)) => Some(d),
        ("readonly", Some(d)) => Some(Arc::new(crate::db::ReadOnly(d))),
        (other, _) => {
            return Err(err(
                400,
                "bad_request",
                format!("`db` must be none, readonly or live, not `{other}`"),
            ))
        }
    };

    let cfg = st.config();
    let reference = format!("wasm:{name}@{version}");
    let (support2, cfg2, r2) = (support.clone(), cfg.wasm.clone(), reference.clone());
    let hook = tokio::task::spawn_blocking(move || support2.bind_concrete(&r2, &cfg2))
        .await
        .map_err(|e| err(500, "internal", e.to_string()))?
        .map_err(|e| err(404, "not_found", format!("{e:#}")))?;

    // Effects a hook queues are reported, never applied: the test shows the plan, it does not execute it.
    ctx.effects.committed = step > Step::Execute;
    let run = hook.run(step, &mut ctx).await;

    let (kind, short, error) = match &run.outcome {
        Outcome::Continue => ("continue", Value::Null, Value::Null),
        Outcome::ShortCircuit(r) => (
            "respond",
            json!({"status": r.status, "headers": r.headers, "body": r.body}),
            Value::Null,
        ),
        Outcome::Fail(e) => (
            "fail",
            Value::Null,
            json!({"status": e.status, "body": e.to_body()}),
        ),
    };
    // contract report: would the response we are about to send satisfy the OpenAPI schema?
    let (mut response_valid, mut problems) = (true, Vec::<String>::new());
    if matches!(run.outcome, Outcome::Continue) {
        if let (Some(v), Some(b)) = (&route.response.validator, &ctx.response.body) {
            if let Err(errs) = v.validate(b) {
                response_valid = false;
                problems = errs.map(|e| format!("{}: {e}", e.instance_path)).collect();
            }
        }
    }
    Ok(Json(json!({
        "reference": reference,
        "pinned": hook.resolved.digest,
        "route": route.id,
        "step": step.as_str(),
        // fail = a deliberate business error from the guest (fine); the rest are defects
        "ok": matches!(run.label, "ok" | "respond" | "fail"),
        "result": run.label,
        "elapsed_ms": run.elapsed.as_secs_f64() * 1000.0,
        "outcome": { "kind": kind, "response": short, "error": error },
        "resulting": {
            "request_body": ctx.body,
            "state": ctx.state,
            "response": { "status": ctx.response.status, "headers": ctx.response.headers, "body": ctx.response.body },
        },
        "contract": { "response_valid": response_valid, "response_problems": problems },
        "effects": { "in_transaction": ctx.effects.in_tx, "after_commit": ctx.effects.after_commit },
        "logs": run.logs,
        "detail": run.detail,
    }))
    .into_response())
}

#[allow(dead_code)]
fn _assert_send(_: &Config) {}
