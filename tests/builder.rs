//! End to end: source text -> builder service (real jco / componentize-py) -> server validation
//! and AOT compile -> bound to a route -> real requests against the database.
//! Point BUILDER_JCO / BUILDER_COMPONENTIZE_PY at the toolchains if they are not on PATH.
mod common;
use axum::Router;
use common::*;
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use stepwise::{
    control, dataplane,
    state::{AppState, ApplyOpts},
};
use stepwise_builder::{router as builder_router, Builder, Config as BuilderConfig};

const TOKEN: &str = "builder-token-0123456789";
const JSON: [(&str, &str); 2] = [
    ("authorization", "Bearer ctl-token"),
    ("content-type", "application/json"),
];

struct Toolchains {
    addr: SocketAddr,
    js: bool,
    py: bool,
}

/// Start a real builder on a local port. `None` if no toolchain is installed (tests then skip).
async fn start_builder() -> Option<Toolchains> {
    let get = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let b = Builder::new(BuilderConfig {
        token: TOKEN.into(),
        jco: get("BUILDER_JCO", "jco").into(),
        componentize_py: get("BUILDER_COMPONENTIZE_PY", "componentize-py").into(),
        wit_dir: format!("{}/wit", env!("CARGO_MANIFEST_DIR")).into(),
        max_source_bytes: 256 * 1024,
        js_timeout: Duration::from_secs(120),
        py_timeout: Duration::from_secs(240),
        max_concurrent: 2,
    })
    .await
    .ok()?;
    let (js, py) = (
        b.toolchains.jco.is_some(),
        b.toolchains.componentize_py.is_some(),
    );
    if !js && !py {
        eprintln!("SKIP: no builder toolchain available");
        return None;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, builder_router(b)).await.ok() });
    Some(Toolchains { addr, js, py })
}

struct Env {
    _dir: tempfile::TempDir,
    st: Arc<AppState>,
    ctl: Router,
    data: Router,
}

fn env(builder: Option<(SocketAddr, &str)>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let mut c = base_config();
    c.database = parse(&format!(
        "enabled: true\npath: '{}'",
        dir.path().join("t.db").display()
    ));
    c.wasm = parse(&format!(
        "enabled: true\ndata_dir: '{}'\nhook_timeout_ms: 2000",
        dir.path().join("w").display()
    ));
    if let Some((addr, token)) = builder {
        c.wasm.builder = parse(&format!(
            "url: 'http://{addr}'\ntoken: '{token}'\ntimeout_secs: 280"
        ));
    }
    let st = state(c);
    Env {
        _dir: dir,
        ctl: control::router(st.clone()),
        data: dataplane::router(st.clone()),
        st,
    }
}

async fn build(e: &Env, name: &str, query: &str, body: Value) -> Resp {
    send(
        &e.ctl,
        "POST",
        &format!("/api/v1/scripts/{name}/source{query}"),
        &JSON,
        Some(&body.to_string()),
    )
    .await
}

async fn bind_pre_process(e: &Env, reference: &str) {
    let mut doc = serde_json::to_value(e.st.config().redacted()).unwrap();
    doc["routes"] =
        json!({ "POST /pets": { "steps": { "pre_process": { "before": [reference] } } } });
    e.st.apply_json(doc, ApplyOpts::default()).await.unwrap();
}

const TS_RULES: &str = r#"
import { log } from "stepwise:hook/host@0.1.0";

export const handler = {
  handle(ctx: any) {
    const body = ctx.requestBody ? JSON.parse(ctx.requestBody) : {};
    if (body.name === "forbidden") {
      return { tag: "fail", val: { status: 422, code: "name_blocked", message: "that name is not allowed" } };
    }
    const name = String(body.name ?? "").trim().replace(/\b\w/g, (c: string) => c.toUpperCase());
    log("info", `normalized to ${name}`);
    return { tag: "proceed", val: { requestBody: JSON.stringify({ ...body, name }) } };
  },
};
"#;

const PY_RULES: &str = r#"
import json
from wit_world import exports
from wit_world.imports import types


class Handler(exports.Handler):
    def handle(self, ctx):
        body = json.loads(ctx.request_body) if ctx.request_body else {}
        body["name"] = str(body.get("name", "")).strip().title() + " (py)"
        return types.Outcome_Proceed(types.Patch(
            state=None, request_body=json.dumps(body),
            response_status=None, response_headers=None, response_body=None))
"#;

#[tokio::test]
async fn typescript_source_becomes_live_endpoint_logic() {
    let Some(tc) = start_builder().await else {
        return;
    };
    if !tc.js {
        return;
    }
    let e = env(Some((tc.addr, TOKEN)));

    // the UI's discovery calls
    let status = send(&e.ctl, "GET", "/api/v1/builder", &JSON, None)
        .await
        .json();
    assert_eq!(
        (status["enabled"].clone(), status["reachable"].clone()),
        (json!(true), json!(true)),
        "{status}"
    );
    assert!(status["builder"]["languages"]
        .as_array()
        .unwrap()
        .contains(&json!("typescript")));
    let tpl = send(&e.ctl, "GET", "/api/v1/builder/templates/ts", &JSON, None)
        .await
        .json();
    assert_eq!(tpl["filename"], "app.ts");
    let ty = send(&e.ctl, "GET", "/api/v1/builder/typings", &JSON, None)
        .await
        .json();
    assert!(
        ty["files"].as_object().is_some_and(|f| f.len() >= 3),
        "typings for editor autocompletion: {ty}"
    );

    // paste source -> version 1
    let r = build(
        &e,
        "pet-rules",
        "",
        json!({"language": "typescript", "source": TS_RULES, "note": "first cut"}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text);
    let j = r.json();
    assert_eq!(
        (
            j["version"].clone(),
            j["reference"].clone(),
            j["deduplicated"].clone()
        ),
        (json!(1), json!("wasm:pet-rules@1"), json!(false))
    );
    assert!(
        j["build"]["toolchain"].as_str().unwrap().contains("jco"),
        "{j}"
    );
    assert!(j["size"].as_u64().unwrap() > 1_000_000, "a real component");

    // the source is kept with the version, byte for byte
    let src = send(
        &e.ctl,
        "GET",
        "/api/v1/scripts/pet-rules/versions/1/source",
        &JSON,
        None,
    )
    .await
    .json();
    assert_eq!(
        (src["language"].clone(), src["source"].as_str().unwrap()),
        (json!("typescript"), TS_RULES)
    );

    // test it before binding it
    let t = send(&e.ctl, "POST", "/api/v1/scripts/pet-rules/versions/1/test", &JSON,
        Some(r#"{"step":"pre_process","request":{"method":"POST","path":"/pets","body":{"name":"  ada lovelace "}}}"#)).await.json();
    assert_eq!(
        (
            t["result"].clone(),
            t["resulting"]["request_body"]["name"].clone()
        ),
        (json!("ok"), json!("Ada Lovelace")),
        "{t}"
    );
    assert!(t["logs"].as_array().unwrap().iter().any(|l| l["message"]
        .as_str()
        .unwrap()
        .contains("normalized to Ada Lovelace")));

    // bind it: pasted code now sits in front of the database
    bind_pre_process(&e, "wasm:pet-rules@1").await;
    let r = send(
        &e.data,
        "POST",
        "/pets",
        &[("content-type", "application/json")],
        Some(r#"{"name":"  grace hopper "}"#),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["name"].clone()),
        (201, json!("Grace Hopper")),
        "{}",
        r.text
    );
    assert_eq!(
        e.st.db
            .as_ref()
            .unwrap()
            .get("pets", "1")
            .await
            .unwrap()
            .unwrap()["name"],
        "Grace Hopper",
        "stored normalized"
    );
    let rej = send(
        &e.data,
        "POST",
        "/pets",
        &[("content-type", "application/json")],
        Some(r#"{"name":"forbidden"}"#),
    )
    .await;
    assert_eq!(
        (rej.status.as_u16(), rej.json()["error"]["code"].clone()),
        (422, json!("name_blocked"))
    );
    assert_eq!(
        e.st.db.as_ref().unwrap().count("pets", &[]).await.unwrap(),
        1,
        "rejected request wrote nothing"
    );

    // same source again: no rebuild, same version
    let again = build(
        &e,
        "pet-rules",
        "",
        json!({"language": "ts", "source": TS_RULES}),
    )
    .await;
    assert_eq!(again.status, 200);
    assert_eq!(
        (
            again.json()["deduplicated"].clone(),
            again.json()["version"].clone()
        ),
        (json!(true), json!(1))
    );
    // changed source -> a new immutable version; v1 keeps serving until you rebind
    let v2 = build(&e, "pet-rules", "", json!({"language": "typescript", "source": TS_RULES.replace("toUpperCase()", "toLowerCase()")})).await;
    assert_eq!(
        (v2.status.as_u16(), v2.json()["version"].clone()),
        (201, json!(2)),
        "{}",
        v2.text
    );
    let r = send(
        &e.data,
        "POST",
        "/pets",
        &[("content-type", "application/json")],
        Some(r#"{"name":"ALAN TURING"}"#),
    )
    .await;
    assert_eq!(
        r.json()["name"],
        "ALAN TURING",
        "route is still pinned to v1 (title-case of an upper-case name leaves it alone)"
    );

    // deleting a version reclaims its source too
    assert_eq!(
        send(
            &e.ctl,
            "DELETE",
            "/api/v1/scripts/pet-rules/versions/2",
            &JSON,
            None
        )
        .await
        .status,
        200
    );
    assert_eq!(
        send(
            &e.ctl,
            "GET",
            "/api/v1/scripts/pet-rules/versions/2/source",
            &JSON,
            None
        )
        .await
        .status,
        404
    );
}

#[tokio::test]
async fn python_source_becomes_live_endpoint_logic() {
    let Some(tc) = start_builder().await else {
        return;
    };
    if !tc.py {
        eprintln!("SKIP: componentize-py not available");
        return;
    }
    let e = env(Some((tc.addr, TOKEN)));
    let r = build(
        &e,
        "py-rules",
        "",
        json!({"language": "python", "source": PY_RULES}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert!(r.json()["build"]["toolchain"]
        .as_str()
        .unwrap()
        .contains("componentize-py"));
    bind_pre_process(&e, "wasm:py-rules@1").await;
    let r = send(
        &e.data,
        "POST",
        "/pets",
        &[("content-type", "application/json")],
        Some(r#"{"name":"  linus torvalds "}"#),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["name"].clone()),
        (201, json!("Linus Torvalds (py)")),
        "{}",
        r.text
    );
    let src = send(
        &e.ctl,
        "GET",
        "/api/v1/scripts/py-rules/versions/1/source",
        &JSON,
        None,
    )
    .await
    .json();
    assert_eq!(src["language"], "python");
}

#[tokio::test]
async fn build_errors_reach_the_caller_and_store_nothing() {
    let Some(tc) = start_builder().await else {
        return;
    };
    let e = env(Some((tc.addr, TOKEN)));
    let nothing_stored = |e: &Env| {
        let c = e.ctl.clone();
        async move {
            send(&c, "GET", "/api/v1/scripts", &JSON, None).await.json()["scripts"]
                .as_array()
                .unwrap()
                .len()
        }
    };
    if tc.js {
        let r = build(&e, "bad", "", json!({"language": "typescript", "source": "export const handler = {\n  handle(ctx) {\n    return {\n"})).await;
        assert_eq!(r.status, 422, "{}", r.text);
        let j = r.json();
        assert_eq!(j["error"]["code"], "build_failed");
        assert!(
            !j["error"]["details"]["log"]
                .as_str()
                .unwrap()
                .trim()
                .is_empty(),
            "the compiler's message reaches the UI: {j}"
        );
        let r = build(
            &e,
            "bad",
            "",
            json!({"language": "javascript", "source": "export const nothing = 1;"}),
        )
        .await;
        assert_eq!(r.status, 422);
        assert!(r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("export `handler`"));
    }
    // request validation happens before any build
    assert_eq!(
        build(&e, "bad", "", json!({"language": "cobol", "source": "x"}))
            .await
            .json()["error"]["code"],
        "bad_language"
    );
    assert_eq!(
        build(&e, "bad", "", json!({"language": "ts"})).await.status,
        400
    );
    assert_eq!(
        build(
            &e,
            "Bad%20Name",
            "",
            json!({"language": "ts", "source": "x"})
        )
        .await
        .status,
        400
    );
    assert_eq!(
        send(
            &e.ctl,
            "POST",
            "/api/v1/scripts/bad/source",
            &JSON,
            Some("nope")
        )
        .await
        .status,
        400
    );
    assert_eq!(
        nothing_stored(&e).await,
        0,
        "failed builds leave no script behind"
    );

    // dry run: built and validated, but nothing is stored
    if tc.js {
        let r = build(
            &e,
            "dry",
            "?dry_run=true",
            json!({"language": "ts", "source": TS_RULES}),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text);
        let digest = r.json()["digest"].as_str().unwrap().to_string();
        assert_eq!(r.json()["dry_run"], true);
        assert_eq!(nothing_stored(&e).await, 0);
        assert!(
            !e.st
                .scripts
                .as_ref()
                .unwrap()
                .wasm_path(&digest)
                .unwrap()
                .exists(),
            "the orphaned artifact was cleaned up"
        );
    }
}

#[tokio::test]
async fn builder_disabled_unreachable_and_misconfigured() {
    // not configured
    let e = env(None);
    assert_eq!(
        send(&e.ctl, "GET", "/api/v1/builder", &JSON, None)
            .await
            .json(),
        json!({"enabled": false})
    );
    let r = build(&e, "x", "", json!({"language": "ts", "source": TS_RULES})).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"]["code"].clone()),
        (409, json!("builder_disabled"))
    );
    assert_eq!(
        send(&e.ctl, "GET", "/api/v1/builder/typings", &JSON, None)
            .await
            .status,
        409
    );

    // configured but nothing is listening
    let dead = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let e = env(Some((dead, TOKEN)));
    let s = send(&e.ctl, "GET", "/api/v1/builder", &JSON, None)
        .await
        .json();
    assert_eq!(
        (s["enabled"].clone(), s["reachable"].clone()),
        (json!(true), json!(false)),
        "{s}"
    );
    let r = build(&e, "x", "", json!({"language": "ts", "source": TS_RULES})).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"]["code"].clone()),
        (502, json!("builder_unreachable")),
        "{}",
        r.text
    );

    // wrong token: reported as a deployment problem (502), not blamed on the caller
    if let Some(tc) = start_builder().await {
        let e = env(Some((tc.addr, "definitely-the-wrong-token")));
        let r = build(&e, "x", "", json!({"language": "ts", "source": TS_RULES})).await;
        assert_eq!(
            (r.status.as_u16(), r.json()["error"]["code"].clone()),
            (502, json!("builder_auth_failed")),
            "{}",
            r.text
        );
    }
}

#[tokio::test]
async fn builder_credentials_are_never_exposed_and_config_is_validated() {
    let dead = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let e = env(Some((dead, "super-secret-builder-token")));
    let cfg = send(&e.ctl, "GET", "/api/v1/config", &JSON, None).await;
    assert!(
        !cfg.text.contains("super-secret-builder-token"),
        "token must be redacted"
    );
    assert_eq!(cfg.json()["config"]["wasm"]["builder"]["token"], "***");
    // round-tripping the redacted config keeps the real secret
    let body = cfg.json()["config"].to_string();
    assert_eq!(
        send(&e.ctl, "PUT", "/api/v1/config", &JSON, Some(&body))
            .await
            .status,
        200
    );
    assert_eq!(
        e.st.config().wasm.builder.token,
        "super-secret-builder-token"
    );

    use stepwise::config::Config;
    let bad = |y: &str| {
        Config::from_yaml_str(&format!("control: {{enabled: false}}\n{y}"))
            .err()
            .map(|e| format!("{e:#}"))
    };
    assert!(bad("wasm: {builder: {url: 'https://b:1', token: x}}")
        .unwrap()
        .contains("http://"));
    assert!(bad("wasm: {builder: {url: 'http://b:1'}}")
        .unwrap()
        .contains("token"));
    assert!(bad("wasm: {builder: {url: 'http://b:1', token: x}}").is_none());
}
