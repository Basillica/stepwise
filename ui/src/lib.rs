//! Stepwise management UI: a static single-page app plus a small authenticating proxy.
//!
//! Why a proxy (backend-for-frontend) instead of letting the page call the control plane?
//!   * the control-plane token never reaches the browser (the page only holds a *UI* token),
//!   * no CORS surface on the control plane,
//!   * one place to add UI-specific protections (login throttling, CSP, no-store).
//!
//! The control plane is reached over plain HTTP: keep the UI and the control plane on a private
//! network (or terminate TLS in front of them).

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::json;

const INDEX: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const APP_CSS: &str = include_str!("../static/app.css");

const MAX_BODY: usize = 128 * 1024 * 1024;
const LOGIN_FAILURES_PER_MINUTE: u32 = 10;

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
                   img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[derive(Debug, Clone)]
pub struct Config {
    /// `host:port` of the control plane (plain HTTP).
    pub control_authority: String,
    /// Bearer token for the control plane. Never sent to the browser.
    pub control_token: String,
    /// What a person types to sign in to this UI.
    pub ui_token: String,
    pub upstream_timeout: Duration,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        use anyhow::Context;
        let url = std::env::var("STEPWISE_CONTROL_URL")
            .context("STEPWISE_CONTROL_URL is required, e.g. http://stepwise:9090")?;
        let authority = url.strip_prefix("http://").context(
            "STEPWISE_CONTROL_URL must start with http:// (keep it on a private network)",
        )?;
        let control_token = std::env::var("STEPWISE_CONTROL_TOKEN")
            .context("STEPWISE_CONTROL_TOKEN is required")?;
        let ui_token = std::env::var("UI_TOKEN").context("UI_TOKEN is required")?;
        if ui_token.len() < 16 {
            anyhow::bail!("UI_TOKEN must be at least 16 characters");
        }
        Ok(Self {
            control_authority: authority.trim_end_matches('/').to_string(),
            control_token,
            ui_token,
            upstream_timeout: Duration::from_secs(330),
        })
    }
}

pub struct State_ {
    cfg: Config,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

type St = Arc<State_>;

pub fn router(cfg: Config) -> Router {
    let st: St = Arc::new(State_ {
        cfg,
        failures: Mutex::new(HashMap::new()),
    });
    Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/healthz",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .fallback(fallback)
        .with_state(st)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut r = Response::new(Body::from(body));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let mut r = (
        status,
        Json(json!({ "error": { "code": code, "message": message.into() } })),
    )
        .into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl State_ {
    fn blocked(&self, ip: IpAddr) -> bool {
        let mut g = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(&ip) {
            Some((n, since)) if since.elapsed() < Duration::from_secs(60) => {
                *n >= LOGIN_FAILURES_PER_MINUTE
            }
            Some(_) => {
                g.remove(&ip);
                false
            }
            None => false,
        }
    }

    fn record_failure(&self, ip: IpAddr) {
        let mut g = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let e = g.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() >= Duration::from_secs(60) {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }
}

async fn fallback(
    State(st): State<St>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    if !path.starts_with("/api/v1/") {
        return err(StatusCode::NOT_FOUND, "not_found", "no such page");
    }
    let lower = path.to_ascii_lowercase();
    if path.split('/').any(|s| s == ".." || s == ".")
        || path.contains('\\')
        || lower.contains("%2e")
        || lower.contains("%2f")
        || lower.contains("%5c")
    {
        return err(StatusCode::BAD_REQUEST, "bad_path", "invalid path");
    }
    if !matches!(
        *req.method(),
        Method::GET | Method::POST | Method::PUT | Method::DELETE
    ) {
        return err(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "only GET, POST, PUT and DELETE are proxied",
        );
    }

    // --- sign-in: checked before comparing, so a blocked IP learns nothing from guessing
    let ip = peer.ip();
    if st.blocked(ip) {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_failures",
            "too many failed sign-ins; wait a minute",
        );
    }
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
    if !presented.is_some_and(|t| ct_eq(t.as_bytes(), st.cfg.ui_token.as_bytes())) {
        st.record_failure(ip);
        return err(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid UI token",
        );
    }

    // --- forward
    let (parts, body) = req.into_parts();
    let bytes = match Limited::new(body, MAX_BODY).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "request body is too large",
            )
        }
    };
    let uri = format!(
        "http://{}{}",
        st.cfg.control_authority,
        parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or(&path)
    );
    let mut up = Request::builder().method(parts.method.clone()).uri(uri);
    // Only a short allow-list is forwarded; the browser's Authorization is replaced, never passed on.
    for name in ["content-type", "if-match", "accept"] {
        if let Some(v) = parts.headers.get(name) {
            up = up.header(name, v);
        }
    }
    let up = match up
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", st.cfg.control_token),
        )
        .body(Full::new(Bytes::from(bytes)))
    {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, "bad_request", e.to_string()),
    };

    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let fut = async {
        let resp = client.request(up).await.map_err(|e| e.to_string())?;
        let (p, b) = resp.into_parts();
        let bytes = Limited::new(b, MAX_BODY)
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok::<_, String>((p, bytes))
    };
    let (p, bytes) = match tokio::time::timeout(st.cfg.upstream_timeout, fut).await {
        Err(_) => {
            return err(
                StatusCode::GATEWAY_TIMEOUT,
                "control_timeout",
                "the control plane did not answer in time",
            )
        }
        Ok(Err(e)) => {
            return err(
                StatusCode::BAD_GATEWAY,
                "control_unreachable",
                format!("cannot reach the control plane: {e}"),
            )
        }
        Ok(Ok(x)) => x,
    };
    if p.status == StatusCode::UNAUTHORIZED {
        // our own credentials to the control plane are wrong: a deployment problem, not "wrong UI token"
        return err(
            StatusCode::BAD_GATEWAY,
            "control_auth_failed",
            "the UI's control-plane token was rejected; check STEPWISE_CONTROL_TOKEN",
        );
    }
    let mut out = Response::new(Body::from(bytes));
    *out.status_mut() = p.status;
    copy_headers(&p.headers, out.headers_mut());
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    out
}

fn copy_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for name in ["content-type", "etag", "retry-after", "location"] {
        if let Some(v) = from.get(name) {
            to.insert(HeaderName::from_static(name), v.clone());
        }
    }
}
