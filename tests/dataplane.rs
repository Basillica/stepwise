mod common;
use common::*;
use serde_json::json;
use stepwise::dataplane;

fn app(cfg: stepwise::config::Config) -> axum::Router {
    dataplane::router(state(cfg))
}

#[tokio::test]
async fn serves_generated_responses_with_security_headers() {
    let a = app(base_config());
    let r = send(&a, "GET", "/pets", &[], None).await;
    assert_eq!(r.status, 200);
    let body = r.json();
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"], "string");
    assert_eq!(body[0]["id"], 1); // minimum: 1
    assert_eq!(body[0]["status"], "available"); // enum[0]
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    assert!(r.header("x-request-id").is_some());
    assert!(
        r.header("strict-transport-security").is_none(),
        "no HSTS without TLS"
    );
    // 204 has no body, example beats generation
    assert_eq!(
        send(&a, "GET", "/health", &[], None).await.json(),
        json!({"status": "ok"})
    );
}

#[tokio::test]
async fn validates_parameters_and_bodies() {
    let a = app(base_config());
    assert_eq!(send(&a, "GET", "/pets/7", &[], None).await.status, 200);
    let bad = send(&a, "GET", "/pets/abc", &[], None).await;
    assert_eq!(bad.status, 400);
    assert_eq!(bad.json()["error"]["code"], "invalid_parameters");
    assert_eq!(send(&a, "GET", "/pets/0", &[], None).await.status, 400); // minimum
    assert_eq!(
        send(&a, "GET", "/pets?limit=0", &[], None).await.status,
        400
    );
    assert_eq!(
        send(&a, "GET", "/pets?limit=5", &[], None).await.status,
        200
    );

    let ok = send(
        &a,
        "POST",
        "/pets",
        &[("content-type", "application/json")],
        Some(r#"{"name":"Rex"}"#),
    )
    .await;
    assert_eq!(ok.status, 201);
    let missing = send(&a, "POST", "/pets", &[], Some(r#"{"tag":"x"}"#)).await;
    assert_eq!(missing.status, 422);
    assert_eq!(missing.json()["error"]["code"], "validation_failed");
    assert_eq!(
        send(&a, "POST", "/pets", &[], Some("{not json"))
            .await
            .json()["error"]["code"],
        "malformed_json"
    );
    assert_eq!(
        send(&a, "POST", "/pets", &[], None).await.json()["error"]["code"],
        "body_required"
    );
}

#[tokio::test]
async fn unknown_paths_and_methods() {
    let a = app(base_config());
    assert_eq!(send(&a, "GET", "/nope", &[], None).await.status, 404);
    let r = send(&a, "PUT", "/pets", &[], None).await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET, POST"));
    assert_eq!(
        send(&a, "GET", "/pets/", &[], None).await.status,
        200,
        "trailing slash tolerated"
    );
}

#[tokio::test]
async fn token_auth_roles_and_openapi_security() {
    let mut c = base_config();
    c.auth = parse(
        "enabled: true\nauth_type: token\nuse_openapi_security: true\nprotected_routes: ['/pets']\n\
         token: {keys: [{value: adm, subject: alice, roles: [admin]}, {value: usr, subject: bob, roles: [user]}]}",
    );
    c.routes =
        parse("\"POST /pets\": {require_roles: [admin]}\n\"GET /pets/{petId}\": {auth: false}");
    let a = app(c);

    assert_eq!(
        send(&a, "GET", "/health", &[], None).await.status,
        200,
        "unprotected stays open"
    );
    assert_eq!(
        send(&a, "GET", "/pets", &[], None).await.status,
        401,
        "protected_routes prefix"
    );
    assert_eq!(
        send(&a, "GET", "/pets", &[("authorization", "Bearer usr")], None)
            .await
            .status,
        200
    );
    assert_eq!(
        send(&a, "GET", "/pets/3", &[], None).await.status,
        200,
        "per-route auth: false wins over prefix"
    );
    assert_eq!(
        send(&a, "DELETE", "/pets/3", &[], None).await.status,
        401,
        "OpenAPI security"
    );
    assert_eq!(
        send(
            &a,
            "DELETE",
            "/pets/3",
            &[("authorization", "Bearer usr")],
            None
        )
        .await
        .status,
        204
    );

    let body = r#"{"name":"Rex"}"#;
    let forbidden = send(
        &a,
        "POST",
        "/pets",
        &[("authorization", "Bearer usr")],
        Some(body),
    )
    .await;
    assert_eq!(forbidden.status, 403);
    assert_eq!(
        forbidden.json()["error"]["details"]["required_any_of"],
        json!(["admin"])
    );
    assert_eq!(
        send(
            &a,
            "POST",
            "/pets",
            &[("authorization", "Bearer adm")],
            Some(body)
        )
        .await
        .status,
        201
    );
    assert_eq!(send(&a, "POST", "/pets", &[], Some(body)).await.status, 401);
}

#[tokio::test]
async fn hooks_run_at_the_right_steps() {
    let mut c = base_config();
    c.logging.step_trace_header = true;
    c.routes = parse(
        "\"GET /pets\":\n  steps:\n    post_process: {after: ['builtin:stamp']}\n\
         \"GET /pets/{petId}\":\n  steps:\n    execute: {replace: 'builtin:echo'}\n\
         \"DELETE /pets/{petId}\":\n  steps:\n    authorize: {before: ['builtin:deny']}",
    );
    let a = app(c);

    let r = send(&a, "GET", "/pets", &[], None).await;
    assert_eq!(r.header("x-stepwise-stamp"), Some("post_process"));
    let trace = r.header("x-stepwise-trace").unwrap();
    assert!(trace.starts_with("rate_limit=ok"), "{trace}");
    assert!(
        trace.contains("execute=ok") && trace.contains("validate_response=ok"),
        "{trace}"
    );

    let echo = send(&a, "GET", "/pets/42?x=1", &[], None).await;
    assert_eq!(echo.json()["path_params"]["petId"], "42");
    assert_eq!(echo.json()["query"], json!([["x", "1"]]));

    let denied = send(&a, "DELETE", "/pets/1", &[], None).await;
    assert_eq!(denied.status, 403);
    let t = denied.header("x-stepwise-trace");
    assert!(
        t.is_none() || !t.unwrap().contains("execute"),
        "short-circuited before execute"
    );
}

#[tokio::test]
async fn config_mistakes_fail_at_build_time() {
    let mut c = base_config();
    c.routes = parse("\"GET /pets\": {steps: {execute: {replace: 'nope:missing'}}}");
    let e = stepwise::state::AppState::new(
        c,
        None,
        stepwise::hooks::HookRegistry::with_builtins(),
        None,
    )
    .err()
    .unwrap();
    assert!(
        format!("{e:#}").contains("unknown hook `nope:missing`"),
        "{e:#}"
    );

    let mut c = base_config();
    c.routes = parse("\"GET /typo\": {auth: true}");
    let e = stepwise::state::AppState::new(
        c,
        None,
        stepwise::hooks::HookRegistry::with_builtins(),
        None,
    )
    .err()
    .unwrap();
    assert!(format!("{e:#}").contains("does not match any"), "{e:#}");

    let mut c = base_config();
    c.routes = parse("\"GET /pets\": {require_roles: [admin]}");
    let e = stepwise::state::AppState::new(
        c,
        None,
        stepwise::hooks::HookRegistry::with_builtins(),
        None,
    )
    .err()
    .unwrap();
    assert!(
        format!("{e:#}").contains("authentication is not enabled"),
        "{e:#}"
    );
}

#[tokio::test]
async fn rate_limiting_per_route_and_default() {
    let mut c = base_config();
    c.routes = parse("\"GET /pets\": {rate_limit: {requests: 2, per_seconds: 60, key: ip}}");
    c.rate_limit = parse("enabled: true\ndefault: {requests: 3, per_seconds: 60, key: ip}");
    let a = app(c);

    // per-route policy: 2 then 429, per client IP
    let r1 = send_from(&a, [10, 0, 0, 1], "GET", "/pets", &[], None).await;
    assert_eq!(
        (r1.status.as_u16(), r1.header("x-ratelimit-remaining")),
        (200, Some("1"))
    );
    assert_eq!(
        send_from(&a, [10, 0, 0, 1], "GET", "/pets", &[], None)
            .await
            .status,
        200
    );
    let blocked = send_from(&a, [10, 0, 0, 1], "GET", "/pets", &[], None).await;
    assert_eq!(blocked.status, 429);
    assert!(blocked.header("retry-after").is_some());
    assert_eq!(blocked.json()["error"]["code"], "rate_limited");
    assert_eq!(
        send_from(&a, [10, 0, 0, 2], "GET", "/pets", &[], None)
            .await
            .status,
        200,
        "other IP unaffected"
    );

    // default policy is shared across routes without their own policy
    for _ in 0..3 {
        assert_eq!(
            send_from(&a, [10, 0, 0, 3], "GET", "/health", &[], None)
                .await
                .status,
            200
        );
    }
    assert_eq!(
        send_from(&a, [10, 0, 0, 3], "GET", "/pets/1", &[], None)
            .await
            .status,
        429
    );
}

#[tokio::test]
async fn ignore_endpoints_and_response_validation_modes() {
    let mut c = base_config();
    c.ignore_endpoints = vec!["^/health".into()];
    assert_eq!(send(&app(c), "GET", "/health", &[], None).await.status, 404);

    let mut warn = base_config();
    warn.openapi.validation.responses = stepwise::config::ResponseValidation::Warn;
    assert_eq!(
        send(&app(warn), "GET", "/broken", &[], None).await.status,
        200
    );
    let mut strict = base_config();
    strict.openapi.validation.responses = stepwise::config::ResponseValidation::Error;
    let r = send(&app(strict), "GET", "/broken", &[], None).await;
    assert_eq!(r.status, 500);
    assert_eq!(r.json()["error"]["code"], "response_contract_violation");
}

#[tokio::test]
async fn request_limits_cors_tracing_and_hsts() {
    let mut c = base_config();
    c.server.max_body_bytes = 16;
    let a = app(c);
    let big = format!(r#"{{"name":"{}"}}"#, "x".repeat(64));
    assert_eq!(send(&a, "POST", "/pets", &[], Some(&big)).await.status, 413);

    let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let r = send(
        &a,
        "GET",
        "/health",
        &[("x-request-id", "abc-123"), ("traceparent", tp)],
        None,
    )
    .await;
    assert_eq!(r.header("x-request-id"), Some("abc-123"));
    assert_eq!(r.header("traceparent"), Some(tp));
    let r = send(
        &a,
        "GET",
        "/health",
        &[("x-request-id", "bad id with spaces")],
        None,
    )
    .await;
    assert_ne!(
        r.header("x-request-id"),
        Some("bad id with spaces"),
        "unsafe ids are replaced"
    );

    let pre = send(
        &a,
        "OPTIONS",
        "/pets",
        &[
            ("origin", "https://ui.example"),
            ("access-control-request-method", "POST"),
        ],
        None,
    )
    .await;
    assert!(pre.header("access-control-allow-origin").is_some());

    let mut t = base_config();
    t.server.tls = parse("enabled: true\ncert_path: x.pem\nkey_path: y.pem");
    let r = send(&app(t), "GET", "/health", &[], None).await;
    assert!(r
        .header("strict-transport-security")
        .unwrap()
        .starts_with("max-age="));
}
