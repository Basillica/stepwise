mod common;
use axum::Router;
use common::*;
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use stepwise::{
    control, dataplane,
    state::{AppState, ApplyOpts},
};

const AUTH: [(&str, &str); 1] = [("authorization", "Bearer ctl-token")];
const JSON: [(&str, &str); 2] = [
    ("authorization", "Bearer ctl-token"),
    ("content-type", "application/json"),
];

struct Env {
    _dir: tempfile::TempDir,
    st: Arc<AppState>,
    ctl: Router,
    data: Router,
}

fn env_with(tweak: impl FnOnce(&mut stepwise::config::Config)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let mut c = base_config();
    c.wasm = parse(&format!(
        "enabled: true\ndata_dir: '{}'\nhook_timeout_ms: 400\nmax_concurrent: 8",
        dir.path().display()
    ));
    tweak(&mut c);
    let st = state(c);
    Env {
        _dir: dir,
        ctl: control::router(st.clone()),
        data: dataplane::router(st.clone()),
        st,
    }
}

fn env() -> Env {
    env_with(|_| {})
}

async fn upload(e: &Env, name: &str, bytes: Vec<u8>, lang: &str) -> Resp {
    send_raw(
        &e.ctl,
        "POST",
        &format!("/api/v1/scripts/{name}/versions?language={lang}"),
        &AUTH,
        bytes,
    )
    .await
}

/// Replace the `routes:` section of the live config (goes through the real apply path).
async fn bind(
    e: &Env,
    routes: Value,
) -> Result<stepwise::state::ApplyReport, stepwise::state::ApplyError> {
    let mut doc = serde_json::to_value(e.st.config().redacted()).unwrap();
    doc["routes"] = routes;
    e.st.apply_json(doc, ApplyOpts::default()).await
}

fn hook(step: &str, slot: &str, refs: &[&str]) -> Value {
    json!({ "steps": { step: { slot: refs } } })
}

async fn with_kitchen_sink() -> Env {
    let e = env();
    let r = upload(&e, "kitchen", fixture("kitchen-sink-qjs.wasm"), "ts").await;
    assert_eq!(r.status, 201, "{}", r.text);
    e
}

#[tokio::test]
async fn upload_validates_the_component_and_is_idempotent() {
    let e = env();
    let r = upload(&e, "kitchen", fixture("kitchen-sink-qjs.wasm"), "ts").await;
    assert_eq!(r.status, 201, "{}", r.text);
    let j = r.json();
    assert_eq!(
        (
            j["version"].clone(),
            j["reference"].clone(),
            j["created"].clone()
        ),
        (json!(1), json!("wasm:kitchen@1"), json!(true))
    );
    assert!(j["digest"].as_str().unwrap().starts_with("sha256:"));
    assert!(j["script_id"].as_str().unwrap().starts_with("scr_"));

    let again = upload(&e, "kitchen", fixture("kitchen-sink-qjs.wasm"), "ts").await;
    assert_eq!(
        (
            again.status.as_u16(),
            again.json()["created"].clone(),
            again.json()["version"].clone()
        ),
        (200, json!(false), json!(1))
    );

    let list = send(&e.ctl, "GET", "/api/v1/scripts", &AUTH, None)
        .await
        .json();
    assert_eq!(list["scripts"][0]["name"], "kitchen");
    assert_eq!(list["scripts"][0]["language"], "ts");

    // rejected uploads store nothing
    assert_eq!(
        upload(&e, "kitchen", b"definitely not wasm".to_vec(), "x")
            .await
            .json()["error"]["code"],
        "not_wasm"
    );
    let core_module = upload(&e, "kitchen", b"\0asm\x01\0\0\0".to_vec(), "x").await;
    assert_eq!(
        core_module.status, 422,
        "an empty *core module* is not a component: {}",
        core_module.text
    );
    assert_eq!(core_module.json()["error"]["code"], "invalid_component");
    assert_eq!(
        upload(&e, "Bad%20Name", fixture("kitchen-sink-qjs.wasm"), "x")
            .await
            .status,
        400
    );
    let d = send(&e.ctl, "GET", "/api/v1/scripts/kitchen", &AUTH, None)
        .await
        .json();
    assert_eq!(d["script"]["versions"].as_array().unwrap().len(), 1);

    // the precompiled form sits next to the artifact
    let digest = j["digest"].as_str().unwrap();
    assert!(e
        .st
        .scripts
        .as_ref()
        .unwrap()
        .cwasm_path(digest)
        .unwrap()
        .exists());
}

#[tokio::test]
async fn pre_process_hook_applies_business_logic() {
    let e = with_kitchen_sink().await;
    let rep = bind(&e, json!({ "POST /pets": {
        "steps": { "pre_process": { "before": ["wasm:kitchen@1"] }, "execute": { "replace": "builtin:echo" } } } }))
        .await.unwrap();
    assert!(rep.applied);

    // execute is replaced by an echo, so the response shows what the hook did to the request
    let r = send(
        &e.data,
        "POST",
        "/pets?mode=normalize",
        &[],
        Some(r#"{"name":"  rex the dog "}"#),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(
        r.json()["body"]["name"],
        "Rex The Dog",
        "hook normalized the name"
    );

    // a deliberate business error from the guest
    let r = send(
        &e.data,
        "POST",
        "/pets?mode=reject",
        &[],
        Some(r#"{"name":"forbidden"}"#),
    )
    .await;
    assert_eq!(r.status, 422);
    let j = r.json();
    assert_eq!(
        (
            j["error"]["code"].clone(),
            j["error"]["details"]["field"].clone()
        ),
        (json!("name_blocked"), json!("name"))
    );
    assert_eq!(
        send(
            &e.data,
            "POST",
            "/pets?mode=reject",
            &[],
            Some(r#"{"name":"fine"}"#)
        )
        .await
        .status,
        200
    );
}

#[tokio::test]
async fn post_process_and_short_circuit() {
    let e = with_kitchen_sink().await;
    bind(&e, json!({
        "GET /pets": hook("post_process", "after", &["wasm:kitchen@1"]),
        "GET /pets/{petId}": { "steps": { "pre_process": { "before": ["wasm:kitchen@1"] } }, "status": 200 },
    })).await.unwrap();

    let r = send(&e.data, "GET", "/pets?mode=post", &[], None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-hook"), Some("ok"));
    assert_eq!(r.json()[0]["decoratedBy"], "post-hook");

    // `respond` short-circuits: execute never runs (the generated Pet would have had an id)
    let r = send(&e.data, "GET", "/pets/5?mode=respond", &[], None).await;
    assert_eq!(r.json(), json!({"cached": true, "step": "pre_process"}));
    assert_eq!(r.header("x-served-by"), Some("hook"));
}

#[tokio::test]
async fn host_rejects_bad_guest_output() {
    let e = with_kitchen_sink().await;
    bind(
        &e,
        json!({ "POST /pets": hook("pre_process", "before", &["wasm:kitchen@1"]) }),
    )
    .await
    .unwrap();
    let post = |q: &'static str| {
        let d = e.data.clone();
        async move {
            send(
                &d,
                "POST",
                &format!("/pets?mode={q}"),
                &[],
                Some(r#"{"name":"Rex"}"#),
            )
            .await
        }
    };

    let r = post("badjson").await;
    assert_eq!(r.status, 500);
    assert_eq!(r.json()["error"]["code"], "hook_contract_violation");
    assert!(r.json()["error"]["details"]["problem"]
        .as_str()
        .unwrap()
        .contains("not valid JSON"));

    let r = post("badschema").await;
    assert_eq!(r.json()["error"]["code"], "hook_contract_violation");
    assert!(
        r.json()["error"]["details"]["problem"]
            .as_str()
            .unwrap()
            .contains("violates the route's schema"),
        "{}",
        r.text
    );
}

#[tokio::test]
async fn crashes_timeouts_and_memory_are_contained() {
    let e = with_kitchen_sink().await;
    bind(
        &e,
        json!({
            "POST /pets": hook("pre_process", "before", &["wasm:kitchen@1"]),
            "GET /pets": hook("pre_process", "before", &["wasm:kitchen@1"]),
        }),
    )
    .await
    .unwrap();

    let t = send(
        &e.data,
        "POST",
        "/pets?mode=throw",
        &[],
        Some(r#"{"name":"x"}"#),
    )
    .await;
    assert_eq!(
        (t.status.as_u16(), t.json()["error"]["code"].clone()),
        (500, json!("hook_failed"))
    );
    assert!(
        !t.text.contains("boom"),
        "guest internals must not leak to clients: {}",
        t.text
    );

    // (memory exhaustion has its own test below: it needs a longer budget than this one)

    // an infinite loop is cut off by the deadline, and does NOT block other requests meanwhile
    let started = Instant::now();
    let (looped, fast) = tokio::join!(
        send(
            &e.data,
            "POST",
            "/pets?mode=loop",
            &[],
            Some(r#"{"name":"x"}"#)
        ),
        async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let r = send(&e.data, "GET", "/health", &[], None).await;
            (r.status.as_u16(), started.elapsed())
        }
    );
    assert_eq!(
        (
            looped.status.as_u16(),
            looped.json()["error"]["code"].clone()
        ),
        (504, json!("hook_timeout"))
    );
    assert!(looped.status == 504 && started.elapsed() < Duration::from_secs(5));
    assert_eq!(fast.0, 200);
    assert!(
        fast.1 < Duration::from_millis(350),
        "other requests were served while the loop spun (took {:?})",
        fast.1
    );

    // and the runtime is still healthy afterwards
    assert_eq!(
        send(&e.data, "GET", "/pets?mode=noop", &[], None)
            .await
            .status,
        200
    );
    let metrics = send(&e.ctl, "GET", "/api/v1/metrics", &AUTH, None)
        .await
        .text;
    for needle in [
        "result=\"trap\"",
        "result=\"timeout\"",
        "stepwise_hook_duration_seconds_count",
    ] {
        assert!(metrics.contains(needle), "metrics missing {needle}");
    }
}

#[tokio::test]
async fn test_endpoint_runs_a_hook_before_it_is_bound() {
    let e = with_kitchen_sink().await;
    let t = |body: Value| {
        let c = e.ctl.clone();
        async move {
            send(
                &c,
                "POST",
                "/api/v1/scripts/kitchen/versions/1/test",
                &JSON,
                Some(&body.to_string()),
            )
            .await
        }
    };

    let r = t(json!({ "step": "pre_process", "request": { "method": "POST", "path": "/pets?mode=normalize", "body": { "name": " ada " } } })).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let j = r.json();
    assert_eq!(
        (j["ok"].clone(), j["result"].clone(), j["route"].clone()),
        (json!(true), json!("ok"), json!("POST /pets"))
    );
    assert_eq!(j["resulting"]["request_body"]["name"], "Ada");
    assert_eq!(j["resulting"]["state"]["normalized"], true);
    assert!(j["pinned"].as_str().unwrap().starts_with("sha256:"));
    assert!(j["elapsed_ms"].as_f64().unwrap() > 0.0);

    // contract report: post-process decoration still satisfies the Pet schema?
    let j = t(json!({ "step": "post_process", "request": { "method": "GET", "path": "/pets?mode=post" } })).await.json();
    assert_eq!(
        j["resulting"]["response"]["body"][0]["decoratedBy"],
        "post-hook"
    );
    assert_eq!(j["contract"]["response_valid"], true);

    // logs from the guest are captured
    let j = t(json!({ "request": { "method": "GET", "path": "/pets?mode=log" } }))
        .await
        .json();
    let logs = j["logs"].as_array().unwrap();
    assert!(
        logs.iter().any(|l| l["level"] == "info"
            && l["message"]
                .as_str()
                .unwrap()
                .contains("hello from GET /pets")),
        "{logs:?}"
    );
    assert!(logs.iter().any(|l| l["level"] == "error"));

    // business failure is "ok"; defects are not
    let j = t(json!({ "request": { "method": "POST", "path": "/pets?mode=reject", "body": { "name": "forbidden" } } })).await.json();
    assert_eq!(
        (
            j["ok"].clone(),
            j["result"].clone(),
            j["outcome"]["kind"].clone()
        ),
        (json!(true), json!("fail"), json!("fail"))
    );
    let j = t(json!({ "request": { "method": "POST", "path": "/pets?mode=throw", "body": { "name": "x" } } })).await.json();
    assert_eq!(
        (j["ok"].clone(), j["result"].clone()),
        (json!(false), json!("trap"))
    );
    assert!(
        j["detail"].as_str().unwrap().contains("wasm backtrace"),
        "testers see the trap detail: {j}"
    );
    let j = t(json!({ "request": { "method": "POST", "path": "/pets?mode=badschema", "body": { "name": "x" } } })).await.json();
    assert_eq!(j["result"], "contract");

    // errors in the test request itself
    assert_eq!(
        t(json!({ "request": { "method": "GET", "path": "/no/such/route" } }))
            .await
            .status,
        422
    );
    assert_eq!(
        t(json!({ "step": "bogus", "request": { "path": "/pets" } }))
            .await
            .status,
        400
    );
    let missing = send(
        &e.ctl,
        "POST",
        "/api/v1/scripts/kitchen/versions/9/test",
        &JSON,
        Some(r#"{"request":{"path":"/pets"}}"#),
    )
    .await;
    assert_eq!(missing.status, 404);
    // nothing was bound or persisted by testing
    assert!(send(&e.ctl, "GET", "/api/v1/routes", &AUTH, None)
        .await
        .json()["routes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["steps"].as_object().unwrap().is_empty()));
}

#[tokio::test]
async fn versions_aliases_pinning_and_rollback() {
    let e = env();
    assert_eq!(
        upload(&e, "kitchen", fixture("kitchen-sink-qjs.wasm"), "ts")
            .await
            .json()["version"],
        1
    );
    assert_eq!(
        upload(&e, "kitchen", fixture("kitchen-sink-b-qjs.wasm"), "ts")
            .await
            .json()["version"],
        2
    );
    let put_alias = |a: &str, v: u32| {
        let (c, a) = (e.ctl.clone(), a.to_string());
        async move {
            send(
                &c,
                "PUT",
                &format!("/api/v1/scripts/kitchen/aliases/{a}"),
                &JSON,
                Some(&json!({"version": v}).to_string()),
            )
            .await
        }
    };
    assert_eq!(put_alias("live", 1).await.status, 200);
    assert_eq!(
        put_alias("live", 7).await.status,
        422,
        "cannot point at a missing version"
    );

    // /pets  follows the alias; /pets/{petId}-style pin is immutable
    bind(&e, json!({
        "POST /pets": { "steps": { "pre_process": { "before": ["wasm:kitchen@live"] }, "execute": { "replace": "builtin:echo" } } },
    })).await.unwrap();
    let name = |e: &Env| {
        let d = e.data.clone();
        async move {
            send(
                &d,
                "POST",
                "/pets?mode=normalize",
                &[],
                Some(r#"{"name":"rex the dog"}"#),
            )
            .await
            .json()["body"]["name"]
                .clone()
        }
    };
    assert_eq!(name(&e).await, "Rex The Dog", "live -> v1");

    // promote: the very next request runs v2, no restart, config unchanged
    let r = put_alias("live", 2).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["previous"].clone()),
        (200, json!(1))
    );
    assert!(
        r.json()["applied"]["config_version"].is_number(),
        "routes follow the alias, so the runtime was rebuilt"
    );
    assert_eq!(name(&e).await, "rex the dog", "live -> v2");
    // instant rollback
    put_alias("live", 1).await;
    assert_eq!(name(&e).await, "Rex The Dog");

    // usage tracking and safety rails
    let d = send(&e.ctl, "GET", "/api/v1/scripts/kitchen", &AUTH, None)
        .await
        .json();
    assert_eq!(d["used_by"][0]["route"], "POST /pets");
    assert_eq!(d["used_by"][0]["ref"], "wasm:kitchen@live");
    let del = send(
        &e.ctl,
        "DELETE",
        "/api/v1/scripts/kitchen/versions/1",
        &AUTH,
        None,
    )
    .await;
    assert_eq!(del.status, 409, "alias + route still use v1: {}", del.text);
    assert_eq!(
        send(
            &e.ctl,
            "DELETE",
            "/api/v1/scripts/kitchen/aliases/live",
            &AUTH,
            None
        )
        .await
        .json()["error"]["code"],
        "in_use"
    );
    assert_eq!(
        send(
            &e.ctl,
            "DELETE",
            "/api/v1/scripts/kitchen/versions/2",
            &AUTH,
            None
        )
        .await
        .status,
        200,
        "v2 is unused"
    );

    // a config that points at something that does not exist is rejected and changes nothing
    let before = e.st.version();
    let bad = bind(
        &e,
        json!({ "POST /pets": hook("pre_process", "before", &["wasm:kitchen@9"]) }),
    )
    .await;
    assert!(bad.unwrap_err().to_string().contains("no version matching"));
    let bad = bind(
        &e,
        json!({ "POST /pets": hook("pre_process", "before", &["wasm:nope@1"]) }),
    )
    .await;
    assert!(bad.unwrap_err().to_string().contains("no such script"));
    assert_eq!(e.st.version(), before);
    assert_eq!(name(&e).await, "Rex The Dog", "old runtime still serving");
}

#[tokio::test]
async fn artifacts_and_index_survive_a_restart() {
    let e = with_kitchen_sink().await;
    send(
        &e.ctl,
        "PUT",
        "/api/v1/scripts/kitchen/aliases/live",
        &JSON,
        Some(r#"{"version":1}"#),
    )
    .await;
    let dir = e.st.config().wasm.data_dir.clone();

    let mut c = base_config();
    c.wasm = parse(&format!("enabled: true\ndata_dir: '{dir}'"));
    c.routes = parse("\"POST /pets\": {steps: {pre_process: {before: ['wasm:kitchen@live']}, execute: {replace: 'builtin:echo'}}}");
    let st2 = state(c); // build resolves + loads from the precompiled .cwasm
    let d2 = dataplane::router(st2);
    let r = send(
        &d2,
        "POST",
        "/pets?mode=normalize",
        &[],
        Some(r#"{"name":"ada lovelace"}"#),
    )
    .await;
    assert_eq!(r.json()["body"]["name"], "Ada Lovelace", "{}", r.text);
}

#[tokio::test]
async fn credentials_are_hidden_from_hooks_unless_enabled() {
    for (expose, expect) in [(false, false), (true, true)] {
        let e = env_with(|c| c.wasm.expose_credentials = expose);
        upload(&e, "kitchen", fixture("kitchen-sink-qjs.wasm"), "ts").await;
        let j = send(
            &e.ctl,
            "POST",
            "/api/v1/scripts/kitchen/versions/1/test",
            &JSON,
            Some(&json!({ "step": "post_process", "request": { "method": "GET", "path": "/health?mode=echo",
                "headers": { "authorization": "Bearer secret", "x-trace": "1" } } }).to_string()),
        )
        .await
        .json();
        let headers = j["resulting"]["response"]["body"]["ctx"]["headers"]
            .as_array()
            .unwrap()
            .clone();
        let has = |n: &str| headers.iter().any(|h| h["name"] == n);
        assert_eq!(
            has("authorization"),
            expect,
            "expose_credentials={expose}: {headers:?}"
        );
        assert!(has("x-trace"));
    }
}

#[tokio::test]
async fn wasm_disabled_is_explicit() {
    let st = state(base_config());
    let ctl = control::router(st.clone());
    let r = send(&ctl, "GET", "/api/v1/scripts", &AUTH, None).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"]["code"].clone()),
        (409, json!("wasm_disabled"))
    );

    let mut c = base_config();
    c.routes = parse("\"GET /pets\": {steps: {pre_process: {before: ['wasm:x@1']}}}");
    let e = stepwise::state::AppState::new(
        c,
        None,
        stepwise::hooks::HookRegistry::with_builtins(),
        None,
    )
    .err()
    .unwrap();
    assert!(format!("{e:#}").contains("wasm.enabled"), "{e:#}");
}

/// Not a benchmark, but guards against accidentally putting compilation on the request path.
#[tokio::test]
async fn per_call_overhead_is_small() {
    let e = with_kitchen_sink().await;
    bind(
        &e,
        json!({ "GET /pets": hook("pre_process", "before", &["wasm:kitchen@1"]) }),
    )
    .await
    .unwrap();
    for _ in 0..5 {
        send(&e.data, "GET", "/pets?mode=noop", &[], None).await; // warm
    }
    let n = 200;
    let t0 = Instant::now();
    for _ in 0..n {
        assert_eq!(
            send(&e.data, "GET", "/pets?mode=noop", &[], None)
                .await
                .status,
            200
        );
    }
    let avg = t0.elapsed() / n;
    eprintln!("avg request with one QuickJS hook: {avg:?}");
    assert!(
        avg < Duration::from_millis(25),
        "hook path is slow: {avg:?} per request"
    );
}

// ---------------------------------------------------------------- other languages (built on demand)

async fn language_roundtrip(file: &str, lang: &str, expect_lang_state: Option<&str>) {
    let Some(bytes) = optional_fixture(file) else {
        eprintln!("SKIP {file}: build it with tests/fixtures/*/build.sh");
        return;
    };
    let e = env_with(|c| c.wasm.hook_timeout_ms = 20_000); // first call includes interpreter start-up
    let r = upload(&e, "pet", bytes, lang).await;
    assert_eq!(r.status, 201, "{}", r.text);
    bind(&e, json!({ "POST /pets": {
        "steps": { "pre_process": { "before": ["wasm:pet@1"] }, "execute": { "replace": "builtin:echo" } } } })).await.unwrap();
    let r = send(
        &e.data,
        "POST",
        "/pets?mode=normalize",
        &[],
        Some(r#"{"name":"  rex the dog "}"#),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.json()["body"]["name"], "Rex The Dog");
    let r = send(
        &e.data,
        "POST",
        "/pets?mode=reject",
        &[],
        Some(r#"{"name":"forbidden"}"#),
    )
    .await;
    assert_eq!(r.json()["error"]["code"], "name_blocked");
    if let Some(l) = expect_lang_state {
        let j = send(&e.ctl, "POST", "/api/v1/scripts/pet/versions/1/test", &JSON,
            Some(r#"{"request":{"method":"POST","path":"/pets?mode=normalize","body":{"name":"x"}}}"#)).await.json();
        assert_eq!(j["resulting"]["state"]["lang"], l, "{j}");
    }
}

#[tokio::test]
async fn python_component_works_end_to_end() {
    language_roundtrip("pet-py.wasm", "python", Some("python")).await;
}

#[tokio::test]
async fn spidermonkey_js_component_works_end_to_end() {
    language_roundtrip("kitchen-sink-sm.wasm", "js", None).await;
}

#[tokio::test]
async fn memory_cap_stops_a_hog() {
    // Generous time budget so we observe the memory limit itself, not the deadline.
    let e = env_with(|c| {
        c.wasm.hook_timeout_ms = 30_000;
        c.wasm.max_memory_mb = 64;
    });
    upload(&e, "kitchen", fixture("kitchen-sink-qjs.wasm"), "ts").await;
    bind(
        &e,
        json!({ "POST /pets": hook("pre_process", "before", &["wasm:kitchen@1"]) }),
    )
    .await
    .unwrap();
    let t0 = Instant::now();
    let r = send(
        &e.data,
        "POST",
        "/pets?mode=memhog",
        &[],
        Some(r#"{"name":"x"}"#),
    )
    .await;
    eprintln!("memhog contained in {:?}", t0.elapsed());
    assert_eq!(
        (r.status.as_u16(), r.json()["error"]["code"].clone()),
        (500, json!("hook_failed")),
        "{}",
        r.text
    );
    let m = send(&e.ctl, "GET", "/api/v1/metrics", &AUTH, None)
        .await
        .text;
    assert!(m.contains("result=\"trap\""));
    assert!(!r.text.contains("memory"), "no internals in client errors");
    assert_eq!(
        send(
            &e.data,
            "POST",
            "/pets?mode=normalize",
            &[],
            Some(r#"{"name":"ok"}"#)
        )
        .await
        .status,
        201,
        "runtime healthy after OOM"
    );
}
