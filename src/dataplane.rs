//! The data plane: whatever the OpenAPI spec says, served through the step pipeline.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method},
    response::{IntoResponse, Response},
    Router,
};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, Any, CorsLayer};

use crate::{
    config::CorsConfig,
    error::ApiError,
    pipeline::{self, RequestContext},
    routes::Lookup,
    state::AppState,
};

pub fn router(state: Arc<AppState>) -> Router {
    let cors = cors_layer(&state.config().server.cors);
    let r = Router::new().fallback(handle).with_state(state);
    match cors {
        Some(c) => r.layer(c),
        None => r,
    }
}

async fn handle(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let started = Instant::now();
    let rt = st.runtime.load_full();
    let cfg = rt.config.clone();
    let (parts, body) = req.into_parts();

    let request_id = parts
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 128
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let client_ip = client_ip(&parts.headers, peer.ip(), cfg.server.trust_forwarded_for);
    let traceparent = parts
        .headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let (mut resp, route_label) = match rt.table.lookup(&parts.method, parts.uri.path()) {
        Lookup::NotFound => (
            ApiError::not_found().into_response(),
            "unmatched".to_string(),
        ),
        Lookup::MethodNotAllowed(allowed) => {
            let allow = allowed
                .iter()
                .map(Method::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            (
                ApiError::new(
                    405,
                    "method_not_allowed",
                    "method not allowed for this path",
                )
                .with_header("allow", allow)
                .into_response(),
                "unmatched".to_string(),
            )
        }
        Lookup::Found(route, params) => {
            let label = route.id.clone();
            let limit = cfg.server.max_body_bytes;
            match axum::body::to_bytes(body, limit).await {
                Err(_) => (
                    ApiError::new(
                        413,
                        "payload_too_large",
                        format!("body exceeds {limit} bytes"),
                    )
                    .into_response(),
                    label,
                ),
                Ok(bytes) => {
                    let mut ctx = RequestContext::new(
                        route,
                        &parts,
                        params,
                        bytes,
                        client_ip,
                        request_id.clone(),
                    );
                    ctx.db = rt.db.clone();
                    ctx.relations = rt.relations.clone();
                    let dur = Duration::from_secs(cfg.server.request_timeout_secs.max(1));
                    let timed_out = tokio::time::timeout(dur, pipeline::run(&rt, &mut ctx))
                        .await
                        .is_err();
                    if timed_out {
                        ctx.error = Some(ApiError::new(504, "timeout", "request timed out"));
                    }
                    (ctx.into_response(cfg.logging.step_trace_header), label)
                }
            }
        }
    };

    let h = resp.headers_mut();
    if cfg.security.headers {
        for (k, v) in [
            ("x-content-type-options", "nosniff"),
            ("x-frame-options", "DENY"),
            ("referrer-policy", "no-referrer"),
            ("cache-control", "no-store"),
        ] {
            if !h.contains_key(k) {
                h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
            }
        }
        if cfg.security.hsts && cfg.server.tls.enabled {
            h.insert(
                header::STRICT_TRANSPORT_SECURITY,
                HeaderValue::from_static("max-age=31536000; includeSubDomains"),
            );
        }
    }
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        h.insert(HeaderName::from_static("x-request-id"), v);
    }
    if let Some(tp) = traceparent
        .as_deref()
        .and_then(|t| HeaderValue::from_str(t).ok())
    {
        h.insert(HeaderName::from_static("traceparent"), tp);
    }

    let elapsed = started.elapsed();
    let status = resp.status().as_u16();
    rt.metrics.observe_request(
        &route_label,
        parts.method.as_str(),
        status,
        elapsed.as_secs_f64(),
    );
    if cfg.logging.access_log {
        tracing::info!(
            target: "stepwise::access",
            request_id = %request_id,
            trace_id = traceparent.as_deref().and_then(trace_id).unwrap_or(""),
            method = %parts.method,
            path = %parts.uri.path(),
            route = %route_label,
            status,
            latency_ms = elapsed.as_secs_f64() * 1000.0,
            client_ip = %client_ip,
            "request"
        );
    }
    resp
}

fn client_ip(headers: &HeaderMap, peer: IpAddr, trust_xff: bool) -> IpAddr {
    if trust_xff {
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
    }
    peer
}

/// W3C `traceparent`: `00-<32 hex trace id>-<16 hex span id>-<2 hex flags>`.
fn trace_id(tp: &str) -> Option<&str> {
    let mut it = tp.split('-');
    let (_, id, span, flags) = (it.next()?, it.next()?, it.next()?, it.next()?);
    (id.len() == 32
        && span.len() == 16
        && flags.len() == 2
        && id.bytes().all(|b| b.is_ascii_hexdigit()))
    .then_some(id)
}

pub fn cors_layer(c: &CorsConfig) -> Option<CorsLayer> {
    if !c.enabled {
        return None;
    }
    let wildcard = |v: &[String]| v.iter().any(|o| o == "*");
    let creds = c.allow_credentials;
    let mut layer = CorsLayer::new();

    layer = layer.allow_origin(if wildcard(&c.allowed_origins) {
        if creds {
            AllowOrigin::mirror_request()
        } else {
            Any.into()
        }
    } else {
        AllowOrigin::list(
            c.allowed_origins
                .iter()
                .filter_map(|o| HeaderValue::from_str(o).ok()),
        )
    });
    layer = layer.allow_methods(if wildcard(&c.allowed_methods) {
        if creds {
            AllowMethods::mirror_request()
        } else {
            Any.into()
        }
    } else {
        AllowMethods::list(
            c.allowed_methods
                .iter()
                .filter_map(|m| Method::from_bytes(m.to_uppercase().as_bytes()).ok()),
        )
    });
    layer = layer.allow_headers(if wildcard(&c.allowed_headers) {
        if creds {
            AllowHeaders::mirror_request()
        } else {
            Any.into()
        }
    } else {
        AllowHeaders::list(
            c.allowed_headers
                .iter()
                .filter_map(|h| HeaderName::from_bytes(h.to_lowercase().as_bytes()).ok()),
        )
    });
    if creds {
        layer = layer.allow_credentials(true);
    }
    Some(layer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_parsing() {
        let ok = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(trace_id(ok), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert_eq!(trace_id("garbage"), None);
        assert_eq!(trace_id("00-short-00f067aa0ba902b7-01"), None);
    }

    #[test]
    fn forwarded_for_only_when_trusted() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 10.0.0.1"),
        );
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(client_ip(&h, peer, false), peer);
        assert_eq!(
            client_ip(&h, peer, true),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
    }
}
