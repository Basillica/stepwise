//! Build hook *source* (TypeScript, JavaScript, Python) into a component via the builder
//! service, then treat it exactly like an uploaded component. The source is kept with the version.

use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use super::scripts::{err, store_component, wasm};
use crate::{
    error::ApiError,
    state::AppState,
    wasm::{
        builder::{BuilderClient, BuilderError},
        store::{sha256_digest, valid_name, SourceInfo},
    },
};

type Resp = Result<Response, Response>;

const MAX_SOURCE_BYTES: usize = 1024 * 1024;

fn canonical_language(l: &str) -> Option<&'static str> {
    match l.to_ascii_lowercase().as_str() {
        "typescript" | "ts" => Some("typescript"),
        "javascript" | "js" => Some("javascript"),
        "python" | "py" => Some("python"),
        _ => None,
    }
}

fn client(st: &AppState) -> Result<BuilderClient, Response> {
    BuilderClient::from_config(&st.config().wasm.builder).ok_or_else(|| {
        err(
            409,
            "builder_disabled",
            "no builder is configured; set `wasm.builder.url` and `wasm.builder.token`",
        )
    })
}

fn builder_error(e: BuilderError) -> Response {
    match e {
        BuilderError::Unreachable(m) => err(
            502,
            "builder_unreachable",
            format!("cannot reach the builder: {m}"),
        ),
        BuilderError::Timeout => err(504, "builder_timeout", "the builder did not answer in time"),
        BuilderError::Protocol(m) => err(502, "builder_protocol", m),
        BuilderError::Rejected {
            status,
            code,
            message,
            log,
        } => {
            let (http, code) = match (status, code.as_str()) {
                (422, _) => (422, "build_failed"),
                (400, _) => (400, "invalid_source"),
                (501, _) => (501, "unsupported_language"),
                (503, _) => (503, "builder_busy"),
                (504, _) => (504, "build_timeout"),
                (401, _) => (502, "builder_auth_failed"), // our credentials are wrong: a deployment problem, not the caller's
                (_, c) => (
                    502,
                    if c.is_empty() {
                        "builder_error"
                    } else {
                        "builder_error"
                    },
                ),
            };
            let mut e = ApiError::new(http, code, message);
            if let Some(l) = log {
                e = e.with_details(json!({ "log": l }));
            }
            e.into_response()
        }
    }
}

pub(super) async fn build_source(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Resp {
    let (_, store, rt) = wasm(&st)?;
    let client = client(&st)?;
    if !valid_name(&name) {
        return Err(err(
            400,
            "bad_name",
            "script name must match [a-z0-9][a-z0-9_-]{0,62}",
        ));
    }
    let req: Value = serde_json::from_slice(&body).map_err(|e| {
        err(
            400,
            "bad_json",
            format!("body must be {{language, source, backend?, note?}}: {e}"),
        )
    })?;
    let language = req
        .get("language")
        .and_then(Value::as_str)
        .and_then(canonical_language)
        .ok_or_else(|| {
            err(
                400,
                "bad_language",
                "language must be typescript, javascript or python",
            )
        })?;
    let source = req
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| err(400, "bad_request", "`source` (string) is required"))?;
    if source.len() > MAX_SOURCE_BYTES {
        return Err(err(
            400,
            "source_too_large",
            format!("source is over {MAX_SOURCE_BYTES} bytes"),
        ));
    }
    let backend = req.get("backend").and_then(Value::as_str);
    let note = req
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let dry_run = q
        .get("dry_run")
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
    let force = q
        .get("force")
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));

    // Same source + same settings => same version; do not rebuild (builds are slow).
    let source_digest = sha256_digest(source.as_bytes());
    if !dry_run && !force {
        if let Some(v) = store.find_by_source(&name, &source_digest, language, backend) {
            let script = store.get(&name).expect("version exists");
            return Ok(Json(json!({
                "name": name, "script_id": script.id, "version": v.version, "digest": v.digest, "size": v.size,
                "language": language, "created": false, "deduplicated": true,
                "reference": format!("wasm:{name}@{}", v.version),
            }))
            .into_response());
        }
    }

    let built = client
        .build(language, source, backend)
        .await
        .map_err(builder_error)?;

    // From here on this is identical to uploading a component.
    let size = built.wasm.len() as u64;
    let digest = store_component(store.clone(), rt.clone(), built.wasm)
        .await
        .map_err(|resp| resp)?;
    let build_info =
        json!({ "duration_ms": built.build_ms, "toolchain": built.toolchain, "log": built.log });

    if dry_run {
        let referenced = store
            .list()
            .values()
            .flat_map(|s| &s.versions)
            .any(|v| v.digest == digest);
        if !referenced {
            for p in [store.wasm_path(&digest), store.cwasm_path(&digest)]
                .into_iter()
                .flatten()
            {
                let _ = std::fs::remove_file(p);
            }
        }
        return Ok(Json(json!({ "ok": true, "dry_run": true, "size": size, "language": language, "digest": digest, "build": build_info })).into_response());
    }

    let src = SourceInfo {
        digest: store
            .put_source(source)
            .map_err(|e| err(500, "internal", format!("{e:#}")))?,
        language: language.into(),
        backend: backend.map(String::from),
        toolchain: built.toolchain,
    };
    let (v, created) = store
        .add_version(&name, &digest, size, language, &note, Some(src))
        .map_err(|e| err(400, "bad_request", format!("{e:#}")))?;
    let script = store.get(&name).expect("just written");
    let mut resp = Json(json!({
        "name": name, "script_id": script.id, "version": v.version, "digest": v.digest, "size": v.size,
        "language": language, "created": created, "deduplicated": false, "source_digest": source_digest,
        "reference": format!("wasm:{name}@{}", v.version), "build": build_info,
    }))
    .into_response();
    *resp.status_mut() = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(resp)
}

pub(super) async fn get_source(
    State(st): State<Arc<AppState>>,
    Path((name, version)): Path<(String, u32)>,
) -> Resp {
    let (_, store, _) = wasm(&st)?;
    let script = store
        .get(&name)
        .ok_or_else(|| err(404, "not_found", format!("no script `{name}`")))?;
    let v = script
        .versions
        .iter()
        .find(|v| v.version == version)
        .ok_or_else(|| {
            err(
                404,
                "not_found",
                format!("script `{name}` has no version {version}"),
            )
        })?;
    let src = v.source.as_ref().ok_or_else(|| {
        err(
            404,
            "no_source",
            "this version was uploaded as a compiled component; there is no source",
        )
    })?;
    let text = store
        .read_source(&src.digest)
        .map_err(|e| err(500, "internal", format!("{e:#}")))?;
    Ok(Json(json!({
        "name": name, "version": version, "language": src.language, "backend": src.backend,
        "toolchain": src.toolchain, "source_digest": src.digest, "source": text,
    }))
    .into_response())
}

/// Is a builder configured, reachable, and what can it build?
pub(super) async fn status(State(st): State<Arc<AppState>>) -> Resp {
    let Some(client) = BuilderClient::from_config(&st.config().wasm.builder) else {
        return Ok(Json(json!({ "enabled": false })).into_response());
    };
    Ok(match client.get_json("/v1/toolchains").await {
        Ok(v) => Json(json!({ "enabled": true, "reachable": true, "builder": v })).into_response(),
        Err(e) => {
            let msg = match e {
                BuilderError::Unreachable(m) | BuilderError::Protocol(m) => m,
                BuilderError::Timeout => "timed out".into(),
                BuilderError::Rejected { message, .. } => message,
            };
            Json(json!({ "enabled": true, "reachable": false, "error": msg })).into_response()
        }
    })
}

pub(super) async fn template(
    State(st): State<Arc<AppState>>,
    Path(language): Path<String>,
) -> Resp {
    let l =
        canonical_language(&language).ok_or_else(|| err(404, "not_found", "unknown language"))?;
    client(&st)?
        .get_json(&format!("/v1/templates/{l}"))
        .await
        .map(|v| Json(v).into_response())
        .map_err(builder_error)
}

pub(super) async fn typings(State(st): State<Arc<AppState>>) -> Resp {
    client(&st)?
        .get_json("/v1/typings")
        .await
        .map(|v| Json(v).into_response())
        .map_err(builder_error)
}
