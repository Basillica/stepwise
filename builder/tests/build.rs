//! Runs against the real toolchains. Point BUILDER_JCO / BUILDER_COMPONENTIZE_PY at them when
//! they are not on PATH; a missing toolchain skips (not fails) its tests.
use std::{sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use stepwise_builder::{router, templates, Builder, Config};
use tower::ServiceExt;

const TOKEN: &str = "test-token-0123456789";

async fn builder(tweak: impl FnOnce(&mut Config)) -> Arc<Builder> {
    let get = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let mut cfg = Config {
        token: TOKEN.into(),
        jco: get("BUILDER_JCO", "jco").into(),
        componentize_py: get("BUILDER_COMPONENTIZE_PY", "componentize-py").into(),
        wit_dir: format!("{}/../wit", env!("CARGO_MANIFEST_DIR")).into(),
        max_source_bytes: 64 * 1024,
        js_timeout: Duration::from_secs(120),
        py_timeout: Duration::from_secs(240),
        max_concurrent: 3,
    };
    tweak(&mut cfg);
    Builder::new(cfg).await.expect("builder")
}

struct R {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}
impl R {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> R {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let req = b
        .body(
            body.map(|v| Body::from(v.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    R {
        status: parts.status,
        headers: parts.headers,
        body: to_bytes(body, 512 * 1024 * 1024).await.unwrap().to_vec(),
    }
}

fn build(app: &Router, lang: &str, source: &str) -> impl std::future::Future<Output = R> {
    let (app, body) = (app.clone(), json!({"language": lang, "source": source}));
    async move { call(&app, "POST", "/v1/build", Some(TOKEN), Some(body)).await }
}

fn is_component(wasm: &[u8]) -> bool {
    // core module: \0asm 01 00 00 00 ; component: \0asm 0d 00 01 00
    wasm.len() > 8 && &wasm[..4] == b"\0asm" && wasm[4..8] == [0x0d, 0x00, 0x01, 0x00]
}

#[tokio::test]
async fn auth_health_and_discovery() {
    let b = builder(|_| {}).await;
    let app = router(b.clone());
    assert_eq!(
        call(&app, "GET", "/v1/health", None, None).await.status,
        200,
        "health is public"
    );
    assert_eq!(
        call(&app, "GET", "/v1/toolchains", None, None).await.status,
        401
    );
    assert_eq!(
        call(&app, "GET", "/v1/toolchains", Some("wrong"), None)
            .await
            .status,
        401
    );
    let t = call(&app, "GET", "/v1/toolchains", Some(TOKEN), None)
        .await
        .json();
    eprintln!("toolchains: {t}");
    assert!(t["languages"].as_array().is_some());

    let wit = call(&app, "GET", "/v1/wit", Some(TOKEN), None).await;
    assert!(String::from_utf8_lossy(&wit.body).contains("package stepwise:hook"));
    for (lang, file) in [
        ("typescript", "app.ts"),
        ("ts", "app.ts"),
        ("javascript", "app.js"),
        ("python", "app.py"),
    ] {
        let j = call(
            &app,
            "GET",
            &format!("/v1/templates/{lang}"),
            Some(TOKEN),
            None,
        )
        .await
        .json();
        assert_eq!(j["filename"], file);
        assert!(j["source"].as_str().unwrap().contains("Stepwise hook"));
    }
    assert_eq!(
        call(&app, "GET", "/v1/templates/cobol", Some(TOKEN), None)
            .await
            .status,
        404
    );
    if b.toolchains.jco.is_some() {
        let ty = call(&app, "GET", "/v1/typings", Some(TOKEN), None)
            .await
            .json();
        assert!(
            ty["files"]["interfaces/stepwise-hook-types.d.ts"]
                .as_str()
                .unwrap()
                .contains("RequestBody")
                || ty["files"].as_object().unwrap().len() >= 3,
            "{ty}"
        );
    }
}

#[tokio::test]
async fn request_validation() {
    let b = builder(|c| c.max_source_bytes = 2000).await;
    let app = router(b);
    let code = |r: &R| r.json()["error"]["code"].as_str().unwrap_or("").to_string();
    let r = build(&app, "cobol", "x").await;
    assert_eq!(
        (r.status.as_u16(), code(&r).as_str()),
        (400, "invalid_request")
    );
    assert_eq!(build(&app, "typescript", "   ").await.status, 400);
    assert_eq!(
        build(&app, "typescript", &"x".repeat(3000)).await.status,
        400,
        "over the source limit"
    );
    let r = call(
        &app,
        "POST",
        "/v1/build",
        Some(TOKEN),
        Some(json!({"language":"python","source":"x=1","backend":"qjs"})),
    )
    .await;
    // qjs is the default so it is accepted for any language; spidermonkey is not valid for Python
    assert_ne!(r.status, StatusCode::BAD_REQUEST);
    let r = call(
        &app,
        "POST",
        "/v1/build",
        Some(TOKEN),
        Some(json!({"language":"python","source":"x=1","backend":"spidermonkey"})),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = call(&app, "POST", "/v1/build", Some(TOKEN), None).await;
    assert_eq!(code(&r), "bad_request");
    assert_eq!(
        call(&app, "POST", "/v1/build", None, Some(json!({})))
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn javascript_and_typescript_templates_build_to_components() {
    let b = builder(|_| {}).await;
    if b.toolchains.jco.is_none() {
        eprintln!("SKIP: jco not available");
        return;
    }
    let app = router(b);
    for (lang, src) in [
        ("typescript", templates::TYPESCRIPT),
        ("javascript", templates::JAVASCRIPT),
    ] {
        let r = build(&app, lang, src).await;
        assert_eq!(
            r.status,
            200,
            "{lang}: {}",
            String::from_utf8_lossy(&r.body)
                .chars()
                .take(600)
                .collect::<String>()
        );
        assert!(is_component(&r.body), "{lang}: not a component");
        assert!(
            r.body.len() < 5 * 1024 * 1024,
            "QuickJS output stays small, got {}",
            r.body.len()
        );
        assert!(r.headers.get("x-build-ms").is_some());
        assert!(r.headers["x-build-toolchain"]
            .to_str()
            .unwrap()
            .contains("jco"));
    }
}

#[tokio::test]
async fn python_template_builds_to_a_component() {
    let b = builder(|_| {}).await;
    if b.toolchains.componentize_py.is_none() {
        eprintln!("SKIP: componentize-py not available");
        return;
    }
    let r = build(&router(b), "python", templates::PYTHON).await;
    assert_eq!(
        r.status,
        200,
        "{}",
        String::from_utf8_lossy(&r.body)
            .chars()
            .take(800)
            .collect::<String>()
    );
    assert!(is_component(&r.body));
}

#[tokio::test]
async fn compile_errors_come_back_with_the_compiler_output() {
    let b = builder(|_| {}).await;
    let app = router(b.clone());
    if b.toolchains.jco.is_some() {
        let r = build(
            &app,
            "typescript",
            "export const handler = {\n  handle(ctx) {\n    return {\n",
        )
        .await;
        assert_eq!(r.status, 422, "{}", String::from_utf8_lossy(&r.body));
        let j = r.json();
        assert_eq!(j["error"]["code"], "build_failed");
        assert!(
            !j["error"]["log"].as_str().unwrap().trim().is_empty(),
            "the UI needs the compiler's message: {j}"
        );
        assert!(
            !j["error"]["log"].as_str().unwrap().contains('\u{1b}'),
            "ANSI colour codes are stripped"
        );

        // valid JS that does not implement the hook interface
        let r = build(&app, "javascript", "export const nothing = 1;\n").await;
        assert_eq!(
            r.status, 422,
            "missing `handler` export must fail the build"
        );
        assert!(r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("export `handler`"));
        assert!(
            r.json()["error"]["log"]
                .as_str()
                .unwrap()
                .contains("export const handler"),
            "the log shows how to fix it"
        );
    }
    if b.toolchains.componentize_py.is_some() {
        let r = build(&app, "python", "def broken(:\n    pass\n").await;
        assert_eq!(r.status, 422);
        assert!(!r.json()["error"]["log"].as_str().unwrap().trim().is_empty());
    }
}

#[tokio::test]
async fn runaway_builds_are_stopped() {
    let b = builder(|c| c.js_timeout = Duration::from_secs(8)).await;
    if b.toolchains.jco.is_none() {
        return;
    }
    let t0 = std::time::Instant::now();
    // top-level code runs at build time (pre-initialisation), so this would hang forever
    let src = "export const handler = { handle(ctx) { return { tag: 'proceed', val: {} }; } };\nwhile (true) {}\n";
    let r = build(&router(b), "javascript", src).await;
    let elapsed = t0.elapsed();
    eprintln!("runaway build ended after {elapsed:?} with {}", r.status);
    assert_eq!(r.status, 504, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["error"]["code"], "build_timeout");
    assert!(elapsed < Duration::from_secs(60));
}

#[tokio::test]
async fn spidermonkey_backend_builds() {
    let b = builder(|_| {}).await;
    if b.toolchains.jco.is_none() {
        return;
    }
    let r = call(&router(b), "POST", "/v1/build", Some(TOKEN), Some(json!({"language":"javascript","source":templates::JAVASCRIPT,"backend":"spidermonkey"}))).await;
    assert_eq!(
        r.status,
        200,
        "{}",
        String::from_utf8_lossy(&r.body)
            .chars()
            .take(600)
            .collect::<String>()
    );
    assert!(
        is_component(&r.body) && r.body.len() > 5 * 1024 * 1024,
        "SpiderMonkey embeds the full engine ({} bytes)",
        r.body.len()
    );
}
