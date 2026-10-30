mod common;
use common::*;
use serde_json::{json, Value};
use std::sync::Arc;
use stepwise::{control, dataplane, hooks::HookRegistry, state::AppState};

const AUTH: [(&str, &str); 1] = [("authorization", "Bearer ctl-token")];

fn apps() -> (Arc<AppState>, axum::Router, axum::Router) {
    let st = state(base_config());
    (
        st.clone(),
        control::router(st.clone()),
        dataplane::router(st),
    )
}

/// A state backed by a real config file (with a `${ENV}` secret) in a temp dir.
fn file_backed() -> (tempfile::TempDir, Arc<AppState>, axum::Router, axum::Router) {
    std::env::set_var("SW_TEST_CTL_TOKEN", "ctl-token");
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(spec_path(), dir.path().join("api.yaml")).unwrap();
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(
        &cfg_path,
        "control:\n  token: ${SW_TEST_CTL_TOKEN}\n  port: 0\nserver:\n  port: 0\nopenapi:\n  spec_path: api.yaml\nlogging:\n  access_log: false\n",
    )
    .unwrap();
    let cfg = stepwise::config::Config::load(&cfg_path).unwrap();
    let st = AppState::new(cfg, Some(cfg_path), HookRegistry::with_builtins(), None).unwrap();
    (
        dir,
        st.clone(),
        control::router(st.clone()),
        dataplane::router(st),
    )
}

async fn get_config(c: &axum::Router) -> (Value, String) {
    let r = send(c, "GET", "/api/v1/config", &AUTH, None).await;
    assert_eq!(r.status, 200);
    (r.json(), r.header("etag").unwrap().to_string())
}

#[tokio::test]
async fn auth_public_endpoints_and_redaction() {
    let (_st, c, _d) = apps();
    assert_eq!(
        send(&c, "GET", "/api/v1/health", &[], None).await.status,
        200
    );
    assert_eq!(
        send(&c, "GET", "/api/v1/ready", &[], None).await.status,
        200
    );
    assert_eq!(
        send(&c, "GET", "/api/v1/openapi.yaml", &[], None)
            .await
            .status,
        200
    );
    assert_eq!(
        send(&c, "GET", "/api/v1/status", &[], None).await.status,
        401
    );
    assert_eq!(
        send(
            &c,
            "GET",
            "/api/v1/status",
            &[("authorization", "Bearer wrong")],
            None
        )
        .await
        .status,
        401
    );

    let (cfg, etag) = get_config(&c).await;
    assert_eq!(etag, "\"1\"");
    assert_eq!(
        cfg["config"]["control"]["token"], "***",
        "secrets are never returned"
    );

    let s = send(&c, "GET", "/api/v1/status", &AUTH, None).await.json();
    assert_eq!(s["routes"], 8);
    assert_eq!(s["spec"]["title"], "Petstore (stepwise demo)");
    let routes = send(&c, "GET", "/api/v1/routes", &AUTH, None).await.json();
    assert!(routes["routes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["id"] == "GET /pets/{petId}"));
    let hooks = send(&c, "GET", "/api/v1/hooks", &AUTH, None).await.json();
    assert!(hooks["hooks"]
        .as_array()
        .unwrap()
        .contains(&json!("builtin:deny")));
    let m = send(&c, "GET", "/api/v1/metrics", &AUTH, None).await;
    assert!(m.text.contains("stepwise_config_version 1"));
}

#[tokio::test]
async fn failed_logins_are_throttled_even_for_the_right_token() {
    let (_st, c, _d) = apps();
    for _ in 0..10 {
        let r = send_from(
            &c,
            [9, 9, 9, 9],
            "GET",
            "/api/v1/status",
            &[("authorization", "Bearer guess")],
            None,
        )
        .await;
        assert_eq!(r.status, 401);
    }
    let r = send_from(&c, [9, 9, 9, 9], "GET", "/api/v1/status", &AUTH, None).await;
    assert_eq!(
        r.status, 429,
        "blocked IPs learn nothing, even with the correct token"
    );
    assert_eq!(
        send_from(&c, [8, 8, 8, 8], "GET", "/api/v1/status", &AUTH, None)
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn dry_run_reports_actions_without_applying() {
    let (st, c, _d) = apps();
    let (mut cfg, _) = get_config(&c).await;
    cfg["config"]["logging"]["level"] = json!("debug");
    cfg["config"]["server"]["port"] = json!(18081);
    cfg["config"]["logging"]["format"] = json!("json");
    let body = cfg["config"].to_string();

    let r = send(&c, "PUT", "/api/v1/config?dry_run=true", &AUTH, Some(&body)).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let rep = r.json();
    assert_eq!(
        (rep["applied"].clone(), rep["dry_run"].clone()),
        (json!(false), json!(true))
    );
    let action = |p: &str| {
        rep["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["path"] == p)
            .map(|c| c["action"].clone())
    };
    assert_eq!(action("logging.level"), Some(json!("hot")));
    assert_eq!(action("server.port"), Some(json!("restart_data_listener")));
    assert_eq!(action("logging.format"), Some(json!("restart_process")));
    assert_eq!(rep["restarts"], json!(["data_plane"]));
    assert_eq!(st.version(), 1, "dry run changes nothing");
    assert_eq!(st.config().server.port, 0);
}

#[tokio::test]
async fn hot_apply_takes_effect_on_the_live_data_plane() {
    let (_dir, st, c, d) = file_backed();
    assert_eq!(send(&d, "GET", "/pets", &[], None).await.status, 200);

    let (mut cfg, etag) = get_config(&c).await;
    cfg["config"]["routes"] = json!({ "GET /pets": { "rate_limit": { "requests": 1, "per_seconds": 60, "key": "global" } } });
    let body = cfg["config"].to_string();
    let r = send(
        &c,
        "PUT",
        "/api/v1/config",
        &[("authorization", "Bearer ctl-token"), ("if-match", &etag)],
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text);
    let rep = r.json();
    assert_eq!(
        (
            rep["applied"].clone(),
            rep["version"].clone(),
            rep["persisted"].clone()
        ),
        (json!(true), json!(2), json!(true))
    );
    assert_eq!(
        rep["restarts"],
        json!([]),
        "route changes are hot: no listener restarts"
    );

    assert_eq!(send(&d, "GET", "/pets", &[], None).await.status, 200);
    assert_eq!(
        send(&d, "GET", "/pets", &[], None).await.status,
        429,
        "new rate limit is live immediately"
    );

    // stale ETag is rejected
    let stale = send(
        &c,
        "PUT",
        "/api/v1/config",
        &[("authorization", "Bearer ctl-token"), ("if-match", &etag)],
        Some(&body),
    )
    .await;
    assert_eq!(stale.status, 412);
    assert_eq!(stale.json()["error"]["details"]["current_version"], 2);
    assert_eq!(st.version(), 2);
}

#[tokio::test]
async fn persistence_keeps_env_placeholders_and_survives_reload() {
    let (dir, _st, c, _d) = file_backed();
    let (mut cfg, _) = get_config(&c).await;
    cfg["config"]["logging"]["level"] = json!("warn");
    let r = send(
        &c,
        "PUT",
        "/api/v1/config",
        &AUTH,
        Some(&cfg["config"].to_string()),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text);

    let on_disk = std::fs::read_to_string(dir.path().join("config.yaml")).unwrap();
    assert!(
        on_disk.contains("${SW_TEST_CTL_TOKEN}"),
        "secret placeholder must survive:\n{on_disk}"
    );
    assert!(on_disk.contains("warn"), "{on_disk}");
    assert!(
        !on_disk.contains("ctl-token"),
        "resolved secret must never be written:\n{on_disk}"
    );
    assert!(
        dir.path().join("config.yaml.bak").exists(),
        "previous file is kept as .bak"
    );
    // the file we wrote is itself a valid config
    let reread = stepwise::config::Config::load(&dir.path().join("config.yaml")).unwrap();
    assert_eq!(reread.logging.level, "warn");
    assert_eq!(reread.control.token, "ctl-token");
}

#[tokio::test]
async fn rejected_configs_change_nothing() {
    let (_dir, st, c, d) = file_backed();
    let before = std::fs::read_to_string(dir_cfg(&_dir)).unwrap();
    let (mut cfg, _) = get_config(&c).await;
    cfg["config"]["routes"] =
        json!({ "GET /pets": { "steps": { "execute": { "replace": "nope:missing" } } } });
    let r = send(
        &c,
        "PUT",
        "/api/v1/config",
        &AUTH,
        Some(&cfg["config"].to_string()),
    )
    .await;
    assert_eq!(r.status, 422);
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown hook"));

    cfg["config"]["routes"] = json!({ "GET /typo": {} });
    assert_eq!(
        send(
            &c,
            "PUT",
            "/api/v1/config",
            &AUTH,
            Some(&cfg["config"].to_string())
        )
        .await
        .status,
        422
    );
    assert_eq!(
        send(&c, "PUT", "/api/v1/config", &AUTH, Some("not json"))
            .await
            .status,
        400
    );

    assert_eq!(st.version(), 1);
    assert_eq!(
        send(&d, "GET", "/pets", &[], None).await.status,
        200,
        "data plane unaffected"
    );
    assert_eq!(
        std::fs::read_to_string(dir_cfg(&_dir)).unwrap(),
        before,
        "file untouched"
    );
}

fn dir_cfg(d: &tempfile::TempDir) -> std::path::PathBuf {
    d.path().join("config.yaml")
}

#[tokio::test]
async fn listener_changes_are_signalled_to_the_supervisor() {
    let (_dir, st, c, _d) = file_backed();
    let rx = st.data_listener.subscribe();
    assert_eq!(rx.borrow().as_ref().unwrap().addr.port(), 0);
    let (mut cfg, _) = get_config(&c).await;
    cfg["config"]["server"]["port"] = json!(18123);
    let r = send(
        &c,
        "PUT",
        "/api/v1/config",
        &AUTH,
        Some(&cfg["config"].to_string()),
    )
    .await
    .json();
    assert_eq!(r["restarts"], json!(["data_plane"]));
    assert_eq!(rx.borrow().as_ref().unwrap().addr.port(), 18123);
    // a hot-only change does not poke the listener
    let mut rx2 = st.data_listener.subscribe();
    rx2.borrow_and_update();
    cfg["config"]["logging"]["level"] = json!("debug");
    send(
        &c,
        "PUT",
        "/api/v1/config",
        &AUTH,
        Some(&cfg["config"].to_string()),
    )
    .await;
    assert!(!rx2.has_changed().unwrap());
}

#[tokio::test]
async fn reload_picks_up_edits_made_on_disk() {
    let (dir, st, c, d) = file_backed();
    assert_eq!(send(&d, "GET", "/extra", &[], None).await.status, 404);
    let spec = std::fs::read_to_string(dir.path().join("api.yaml"))
        .unwrap()
        .replace(
            "  /broken:",
            "  /extra:\n    get:\n      responses:\n        '200': {description: ok}\n  /broken:",
        );
    std::fs::write(dir.path().join("api.yaml"), spec).unwrap();
    let r = send(&c, "POST", "/api/v1/reload", &AUTH, None).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(send(&d, "GET", "/extra", &[], None).await.status, 200);
    assert_eq!(st.version(), 2);

    std::fs::write(dir.path().join("api.yaml"), "not: openapi").unwrap();
    assert_eq!(
        send(&c, "POST", "/api/v1/reload", &AUTH, None).await.status,
        422
    );
    assert_eq!(
        send(&d, "GET", "/extra", &[], None).await.status,
        200,
        "bad file leaves the old runtime serving"
    );
}

/// Every documented operation is routed, and every routed operation is documented.
#[tokio::test]
async fn control_plane_openapi_matches_the_router() {
    let doc: Value = serde_yaml::from_str(control::OPENAPI_YAML).unwrap();
    let mut documented = std::collections::BTreeSet::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for m in ["get", "put", "post", "delete", "patch"] {
            if item.get(m).is_some() {
                documented.insert((m.to_string(), path.clone()));
            }
        }
    }
    assert_eq!(
        documented,
        control::routes_served(),
        "openapi/control-plane.yaml and control.rs have drifted"
    );

    let (_st, c, _d) = apps();
    for (m, p) in &documented {
        let concrete = p
            .replace("{name}", "x")
            .replace("{version}", "1")
            .replace("{alias}", "live")
            .replace("{language}", "typescript");
        let r = send(
            &c,
            &m.to_uppercase(),
            &format!("/api/v1{concrete}"),
            &AUTH,
            Some("{}"),
        )
        .await;
        assert!(
            r.status != 404 && r.status != 405,
            "{m} {p} is documented but returned {}",
            r.status
        );
    }
    // the spec we serve is itself parseable by our own loader
    let tmp = tempfile::NamedTempFile::with_suffix(".yaml").unwrap();
    std::fs::write(tmp.path(), control::OPENAPI_YAML).unwrap();
    stepwise::spec::load_spec(tmp.path()).unwrap();
}
