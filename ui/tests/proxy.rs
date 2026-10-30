use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Router,
};
use stepwise_ui::{router, Config};
use tower::ServiceExt;

const UI_TOKEN: &str = "ui-token-0123456789abc";
const CONTROL_TOKEN: &str = "control-secret-xyz";

#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// A stand-in control plane that records what it receives.
async fn upstream() -> (SocketAddr, Arc<Mutex<Vec<Seen>>>) {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    async fn handle(State(seen): State<Arc<Mutex<Vec<Seen>>>>, req: Request) -> Response {
        let (parts, body) = req.into_parts();
        let body = to_bytes(body, 256 * 1024 * 1024).await.unwrap().to_vec();
        seen.lock().unwrap().push(Seen {
            method: parts.method.clone(),
            uri: parts.uri.clone(),
            headers: parts.headers.clone(),
            body: body.clone(),
        });
        match parts.uri.path() {
            "/api/v1/unauth" => (StatusCode::UNAUTHORIZED, "no").into_response(),
            "/api/v1/binary" => (
                [("content-type", "application/wasm")],
                (0..=255u8).cycle().take(300_000).collect::<Vec<u8>>(),
            )
                .into_response(),
            "/api/v1/echo" => (
                [
                    ("content-type", "application/json"),
                    ("etag", "\"7\""),
                    ("set-cookie", "leak=1"),
                    ("x-internal", "secret"),
                ],
                body,
            )
                .into_response(),
            _ => ([("content-type", "application/json")], r#"{"ok":true}"#).into_response(),
        }
    }
    let app = Router::new().fallback(handle).with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.ok() });
    (addr, seen)
}

fn ui(addr: SocketAddr) -> Router {
    router(Config {
        control_authority: addr.to_string(),
        control_token: CONTROL_TOKEN.into(),
        ui_token: UI_TOKEN.into(),
        upstream_timeout: Duration::from_secs(10),
    })
}

struct R {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

async fn call(
    app: &Router,
    ip: [u8; 4],
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> R {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::from(body)).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((ip, 5000))));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (p, b) = resp.into_parts();
    R {
        status: p.status,
        headers: p.headers,
        body: to_bytes(b, 256 * 1024 * 1024).await.unwrap().to_vec(),
    }
}

fn authed() -> [(&'static str, String); 1] {
    [("authorization", format!("Bearer {UI_TOKEN}"))]
}

async fn get(app: &Router, uri: &str) -> R {
    let a = authed();
    call(
        app,
        [10, 0, 0, 1],
        "GET",
        uri,
        &[(a[0].0, a[0].1.as_str())],
        vec![],
    )
    .await
}

#[tokio::test]
async fn serves_the_app_with_a_strict_csp_and_needs_no_login_for_assets() {
    let (addr, _) = upstream().await;
    let app = ui(addr);
    for (path, ct) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/app.css", "text/css"),
    ] {
        let r = call(&app, [10, 0, 0, 1], "GET", path, &[], vec![]).await;
        assert_eq!(r.status, 200, "{path}");
        assert!(r.headers["content-type"].to_str().unwrap().starts_with(ct));
        let csp = r.headers["content-security-policy"].to_str().unwrap();
        assert!(
            csp.contains("default-src 'none'")
                && csp.contains("script-src 'self'")
                && csp.contains("frame-ancestors 'none'"),
            "{csp}"
        );
        assert_eq!(r.headers["x-content-type-options"], "nosniff");
    }
    let html = String::from_utf8(
        call(&app, [10, 0, 0, 1], "GET", "/", &[], vec![])
            .await
            .body,
    )
    .unwrap();
    assert!(
        !html.contains("<script>") && html.contains("src=\"/app.js\""),
        "no inline script: the CSP forbids it"
    );
    assert_eq!(
        call(&app, [10, 0, 0, 1], "GET", "/healthz", &[], vec![])
            .await
            .status,
        200
    );
    assert_eq!(
        call(&app, [10, 0, 0, 1], "GET", "/nope", &[], vec![])
            .await
            .status,
        404
    );
    assert_eq!(
        call(&app, [10, 0, 0, 1], "GET", "/api/v2/x", &[], vec![])
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn the_control_token_is_injected_and_browser_credentials_never_reach_it() {
    let (addr, seen) = upstream().await;
    let app = ui(addr);
    let r = call(&app, [10, 0, 0, 1], "GET", "/api/v1/status", &[], vec![]).await;
    assert_eq!(r.status, 401);
    let r = call(
        &app,
        [10, 0, 0, 1],
        "GET",
        "/api/v1/status",
        &[("authorization", "Bearer nope")],
        vec![],
    )
    .await;
    assert_eq!(r.status, 401);
    assert!(
        seen.lock().unwrap().is_empty(),
        "unauthenticated calls never reach the control plane"
    );

    let a = authed();
    let r = call(
        &app,
        [10, 0, 0, 1],
        "GET",
        "/api/v1/status",
        &[
            (a[0].0, a[0].1.as_str()),
            ("cookie", "session=abc"),
            ("x-forwarded-for", "6.6.6.6"),
            ("x-evil", "1"),
        ],
        vec![],
    )
    .await;
    assert_eq!(r.status, 200);
    let s = seen.lock().unwrap()[0].clone();
    assert_eq!(
        s.headers["authorization"],
        format!("Bearer {CONTROL_TOKEN}").as_str(),
        "the control token is injected server-side"
    );
    assert!(
        !format!("{:?}", s.headers).contains(UI_TOKEN),
        "the UI token is not forwarded"
    );
    for h in ["cookie", "x-forwarded-for", "x-evil"] {
        assert!(s.headers.get(h).is_none(), "{h} must not be forwarded");
    }
    assert_eq!(r.headers["cache-control"], "no-store");
}

#[tokio::test]
async fn requests_and_responses_are_forwarded_faithfully() {
    let (addr, seen) = upstream().await;
    let app = ui(addr);
    let a = authed();
    let r = call(
        &app,
        [10, 0, 0, 1],
        "PUT",
        "/api/v1/echo?dry_run=true&x=a%20b",
        &[
            (a[0].0, a[0].1.as_str()),
            ("content-type", "application/json"),
            ("if-match", "\"6\""),
        ],
        br#"{"hello":"world"}"#.to_vec(),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, br#"{"hello":"world"}"#);
    assert_eq!(
        (
            r.headers["etag"].to_str().unwrap(),
            r.headers["content-type"].to_str().unwrap()
        ),
        ("\"7\"", "application/json")
    );
    assert!(
        r.headers.get("set-cookie").is_none() && r.headers.get("x-internal").is_none(),
        "only an allow-list of response headers is passed back"
    );
    let s = seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        (s.method, s.uri.to_string().as_str()),
        (Method::PUT, "/api/v1/echo?dry_run=true&x=a%20b")
    );
    assert_eq!(
        (
            s.headers["if-match"].to_str().unwrap(),
            s.headers["content-type"].to_str().unwrap()
        ),
        ("\"6\"", "application/json")
    );
}

#[tokio::test]
async fn binary_bodies_survive_untouched_in_both_directions() {
    let (addr, seen) = upstream().await;
    let app = ui(addr);
    let a = authed();
    let wasm: Vec<u8> = (0..=255u8).cycle().take(2_000_000).collect();
    let r = call(
        &app,
        [10, 0, 0, 1],
        "POST",
        "/api/v1/scripts/x/versions",
        &[
            (a[0].0, a[0].1.as_str()),
            ("content-type", "application/wasm"),
        ],
        wasm.clone(),
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(
        seen.lock().unwrap().last().unwrap().body == wasm,
        "upload arrived byte-for-byte"
    );
    let r = get(&app, "/api/v1/binary").await;
    assert_eq!(
        r.body,
        (0..=255u8).cycle().take(300_000).collect::<Vec<u8>>()
    );
    assert_eq!(r.headers["content-type"], "application/wasm");
}

#[tokio::test]
async fn upstream_problems_are_reported_as_deployment_problems() {
    let (addr, _) = upstream().await;
    let r = get(&ui(addr), "/api/v1/unauth").await;
    assert_eq!(
        r.status, 502,
        "a rejected control token is not the user's fault (and must not log them out)"
    );
    assert!(String::from_utf8_lossy(&r.body).contains("control_auth_failed"));

    let dead = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let r = get(&ui(dead), "/api/v1/status").await;
    assert_eq!(r.status, 502);
    assert!(String::from_utf8_lossy(&r.body).contains("control_unreachable"));
}

#[tokio::test]
async fn path_tricks_and_odd_methods_are_refused() {
    let (addr, seen) = upstream().await;
    let app = ui(addr);
    for p in [
        "/api/v1/../admin",
        "/api/v1/%2e%2e/admin",
        "/api/v1/a%2Fb",
        "/api/v1/a%5Cb",
    ] {
        assert_eq!(get(&app, p).await.status, 400, "{p}");
    }
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(
        call(&app, [10, 0, 0, 1], "PATCH", "/api/v1/config", &[], vec![])
            .await
            .status,
        405
    );
    assert_eq!(
        call(&app, [10, 0, 0, 1], "TRACE", "/api/v1/config", &[], vec![])
            .await
            .status,
        405
    );
}

#[tokio::test]
async fn sign_in_guessing_is_throttled_per_address() {
    let (addr, _) = upstream().await;
    let app = ui(addr);
    for _ in 0..10 {
        assert_eq!(
            call(
                &app,
                [9, 9, 9, 9],
                "GET",
                "/api/v1/status",
                &[("authorization", "Bearer guess")],
                vec![]
            )
            .await
            .status,
            401
        );
    }
    let a = authed();
    let r = call(
        &app,
        [9, 9, 9, 9],
        "GET",
        "/api/v1/status",
        &[(a[0].0, a[0].1.as_str())],
        vec![],
    )
    .await;
    assert_eq!(
        r.status, 429,
        "a blocked address learns nothing, even with the right token"
    );
    let r = call(
        &app,
        [8, 8, 8, 8],
        "GET",
        "/api/v1/status",
        &[(a[0].0, a[0].1.as_str())],
        vec![],
    )
    .await;
    assert_eq!(r.status, 200, "other addresses are unaffected");
}
