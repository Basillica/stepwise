//! Scripts describe writes ("effects"); the host applies them atomically with the route's own
//! database operation. HTTP-level tests with real WASM hooks (the test guest's `fx-*` modes).
//! Set STEPWISE_TEST_POSTGRES_URL to run them against PostgreSQL.
mod common;
use axum::Router;
use common::*;
use serde_json::{json, Value};
use std::sync::Arc;
use stepwise::{
    control, dataplane,
    state::{AppState, ApplyOpts},
};

const AUTH: [(&str, &str); 1] = [("authorization", "Bearer ctl-token")];
const JSON: [(&str, &str); 1] = [("content-type", "application/json")];
const CTL_JSON: [(&str, &str); 2] = [
    ("authorization", "Bearer ctl-token"),
    ("content-type", "application/json"),
];

struct Env {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    st: Arc<AppState>,
    data: Router,
    ctl: Router,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let mut c = base_config();
    c.database = match std::env::var("STEPWISE_TEST_POSTGRES_URL") {
        Ok(url) => parse(&format!(
            "enabled: true\ndb_type: postgres\nurl: '{url}'\nschema: 't_{}'\npool_size: 6",
            uuid::Uuid::new_v4().simple()
        )),
        Err(_) => parse(&format!(
            "enabled: true\npath: '{}'",
            dir.path().join("t.db").display()
        )),
    };
    c.wasm = parse(&format!(
        "enabled: true\ndata_dir: '{}'\nhook_timeout_ms: 2000",
        dir.path().join("w").display()
    ));
    let st = state(c);
    Env {
        dir,
        data: dataplane::router(st.clone()),
        ctl: control::router(st.clone()),
        st,
    }
}

async fn setup(routes: Value) -> Env {
    let e = env();
    let up = send_raw(
        &e.ctl,
        "POST",
        "/api/v1/scripts/kitchen/versions?language=ts",
        &AUTH,
        fixture("kitchen-sink-qjs.wasm"),
    )
    .await;
    assert_eq!(up.status, 201, "{}", up.text);
    let mut doc = serde_json::to_value(e.st.config().redacted()).unwrap();
    doc["routes"] = routes;
    e.st.apply_json(doc, ApplyOpts::default()).await.unwrap();
    e.st.db
        .as_ref()
        .unwrap()
        .insert("products", "sku-1", &json!({"id": "sku-1", "stock": 5}))
        .await
        .unwrap();
    e
}

fn pre_process_route() -> Value {
    json!({ "POST /pets": {
        "steps": { "pre_process": { "before": ["wasm:kitchen@1"] } },
        "db": { "grants": { "products": "rw", "audit": "rw" } } } })
}

async fn post(e: &Env, path: &str, body: Value) -> Resp {
    send(&e.data, "POST", path, &JSON, Some(&body.to_string())).await
}

async fn count(e: &Env, coll: &str) -> u64 {
    e.st.db.as_ref().unwrap().count(coll, &[]).await.unwrap()
}

async fn stock(e: &Env) -> i64 {
    e.st.db
        .as_ref()
        .unwrap()
        .get("products", "sku-1")
        .await
        .unwrap()
        .unwrap()["stock"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn a_scripts_writes_commit_atomically_with_the_routes_own_write() {
    let e = setup(pre_process_route()).await;

    // all three writes happen: the pet, the stock change, the audit row
    let r = post(
        &e,
        "/pets?mode=fx-order&sku=sku-1&qty=2",
        json!({"name": "Rex"}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(
        (
            stock(&e).await,
            count(&e, "pets").await,
            count(&e, "audit").await
        ),
        (3, 1, 1)
    );

    // the script's precondition fails: its own error reaches the client and NOTHING is written
    let r = post(
        &e,
        "/pets?mode=fx-order&sku=sku-1&qty=9",
        json!({"name": "Tom"}),
    )
    .await;
    assert_eq!(
        (
            r.status.as_u16(),
            r.json()["error"]["code"].clone(),
            r.json()["error"]["message"].clone()
        ),
        (409, json!("out_of_stock"), json!("not enough stock")),
        "{}",
        r.text
    );
    assert_eq!(
        (
            stock(&e).await,
            count(&e, "pets").await,
            count(&e, "audit").await
        ),
        (3, 1, 1),
        "no pet, no stock change, no audit row"
    );

    // the *route's own* write fails (duplicate id): the script's writes are undone with it
    let r = post(
        &e,
        "/pets?mode=fx-order&sku=sku-1&qty=1",
        json!({"id": 1, "name": "Dup"}),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"]["code"].clone()),
        (409, json!("conflict"))
    );
    assert_eq!(
        (stock(&e).await, count(&e, "audit").await),
        (3, 1),
        "the stock decrement was rolled back with the failed insert"
    );

    let m = send(&e.ctl, "GET", "/api/v1/metrics", &AUTH, None)
        .await
        .text;
    assert!(
        m.contains("stepwise_effects_total{outcome=\"in_tx_committed\"} 2"),
        "{m}"
    );
    assert!(
        m.contains("stepwise_effects_total{outcome=\"in_tx_rolled_back\"}"),
        "{m}"
    );
}

#[tokio::test]
async fn a_hook_that_fails_writes_nothing() {
    let e = setup(pre_process_route()).await;
    let r = post(&e, "/pets?mode=fx-then-fail", json!({"name": "Rex"})).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"]["code"].clone()),
        (422, json!("name_blocked"))
    );
    assert_eq!(
        (count(&e, "audit").await, count(&e, "pets").await),
        (0, 0),
        "effects queued before `fail` were discarded"
    );
}

#[tokio::test]
async fn a_hook_that_answers_early_still_gets_its_writes() {
    let e = setup(pre_process_route()).await;
    let r = post(&e, "/pets?mode=fx-respond", json!({"name": "Rex"})).await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (200, json!({"cached": true}))
    );
    assert_eq!(
        (count(&e, "audit").await, count(&e, "pets").await),
        (1, 0),
        "the audit row was written; the route's own write never ran"
    );
}

#[tokio::test]
async fn effects_are_validated_when_emitted_and_limited_to_granted_collections() {
    let e = setup(pre_process_route()).await;
    let probe = |query: &'static str| {
        let c = e.ctl.clone();
        async move {
            let body = json!({ "step": "pre_process", "request": { "method": "POST", "path": format!("/pets?{query}"), "body": { "name": "x" } } });
            let r = send(
                &c,
                "POST",
                "/api/v1/scripts/kitchen/versions/1/test",
                &CTL_JSON,
                Some(&body.to_string()),
            )
            .await;
            assert_eq!(r.status, 200, "{}", r.text);
            r.json()
        }
    };

    // malformed effects are refused at emit() with a message the script can catch
    let j = probe("mode=fx-bad").await;
    assert_eq!(j["result"], "ok", "{j}");
    let emit = j["resulting"]["state"]["emit"].as_array().unwrap().clone();
    assert!(
        emit[0]["err"]
            .as_str()
            .unwrap()
            .contains("not a valid effect"),
        "{emit:?}"
    );
    assert!(
        emit[1]["err"].as_str().unwrap().contains("at least one"),
        "{emit:?}"
    );
    assert!(
        j["effects"]["in_transaction"]
            .as_array()
            .unwrap()
            .is_empty(),
        "refused effects are not queued"
    );

    // collections the route has no grant for are refused
    let j = probe("mode=fx-denied").await;
    assert!(
        j["resulting"]["state"]["emit"][0]["err"]
            .as_str()
            .unwrap()
            .contains("not granted"),
        "{j}"
    );

    // a test shows the plan and applies nothing
    let j = probe("mode=fx-order&sku=sku-1&qty=2").await;
    let plan = j["effects"]["in_transaction"].as_array().unwrap();
    assert_eq!(plan.len(), 2, "{j}");
    assert_eq!(
        (
            plan[0]["op"].clone(),
            plan[0]["collection"].clone(),
            plan[0]["inc"]["stock"].clone()
        ),
        (json!("update"), json!("products"), json!(-2))
    );
    assert_eq!(plan[1]["collection"], "audit");
    assert!(j["effects"]["after_commit"].as_array().unwrap().is_empty());
    assert_eq!(
        (stock(&e).await, count(&e, "audit").await),
        (5, 0),
        "testing a hook never executes its effects"
    );
}

#[tokio::test]
async fn effects_queued_after_the_write_are_applied_best_effort() {
    let e = setup(json!({ "POST /pets": {
        "steps": { "post_process": { "after": ["wasm:kitchen@1"] } },
        "db": { "grants": { "audit": "rw" } } } }))
    .await;

    let r = post(&e, "/pets?mode=fx-after", json!({"name": "Rex"})).await;
    assert_eq!(r.status, 201, "{}", r.text);
    let audit =
        e.st.db
            .as_ref()
            .unwrap()
            .list(
                "audit",
                &stepwise::db::ListQuery {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    assert_eq!(
        (
            audit.len(),
            audit[0]["event"].clone(),
            audit[0]["step"].clone()
        ),
        (1, json!("responded"), json!("post_process"))
    );

    // a failing after-commit effect never changes the response
    let r = post(&e, "/pets?mode=fx-after-bad", json!({"name": "Tom"})).await;
    assert_eq!(r.status, 201, "the client still gets its pet: {}", r.text);
    assert_eq!(count(&e, "pets").await, 2);
    assert_eq!(count(&e, "audit").await, 1);
    let m = send(&e.ctl, "GET", "/api/v1/metrics", &AUTH, None)
        .await
        .text;
    assert!(
        m.contains("stepwise_effects_total{outcome=\"after_commit_failed\"} 1"),
        "{m}"
    );
    assert!(
        m.contains("stepwise_effects_total{outcome=\"after_commit_ok\"} 1"),
        "{m}"
    );
}

#[tokio::test]
async fn etag_and_if_match_protect_against_lost_updates_over_http() {
    let e = env();
    let r = post(&e, "/pets", json!({"name": "A"})).await;
    assert_eq!((r.status.as_u16(), r.header("etag")), (201, Some("\"1\"")));
    assert_eq!(
        send(&e.data, "GET", "/pets/1", &[], None)
            .await
            .header("etag"),
        Some("\"1\"")
    );

    let put = |body: &'static str, if_match: Option<&'static str>| {
        let d = e.data.clone();
        async move {
            let mut h = vec![("content-type", "application/json")];
            if let Some(m) = if_match {
                h.push(("if-match", m));
            }
            send(&d, "PUT", "/pets/1", &h, Some(body)).await
        }
    };
    let r = put(r#"{"name":"B"}"#, Some("\"1\"")).await;
    assert_eq!((r.status.as_u16(), r.header("etag")), (200, Some("\"2\"")));

    // someone else read version 1 and tries to write: refused, told the current version
    let r = put(r#"{"name":"C"}"#, Some("\"1\"")).await;
    assert_eq!(
        (
            r.status.as_u16(),
            r.json()["error"]["code"].clone(),
            r.header("etag")
        ),
        (412, json!("precondition_failed"), Some("\"2\""))
    );
    assert_eq!(
        send(&e.data, "GET", "/pets/1", &[], None).await.json()["name"],
        "B",
        "the stale write changed nothing"
    );

    let r = send(
        &e.data,
        "PATCH",
        "/pets/1",
        &[("content-type", "application/json"), ("if-match", "\"2\"")],
        Some(r#"{"tag":"x"}"#),
    )
    .await;
    assert_eq!((r.status.as_u16(), r.header("etag")), (200, Some("\"3\"")));
    assert_eq!(
        send(
            &e.data,
            "PUT",
            "/pets/1",
            &[("content-type", "application/json"), ("if-match", "bogus")],
            Some(r#"{"name":"D"}"#)
        )
        .await
        .status,
        400
    );
    assert_eq!(
        send(&e.data, "DELETE", "/pets/1", &[("if-match", "\"9\"")], None)
            .await
            .status,
        412
    );
    assert_eq!(
        send(&e.data, "DELETE", "/pets/1", &[("if-match", "\"3\"")], None)
            .await
            .status,
        204
    );
}
