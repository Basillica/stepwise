//! The read language through the control plane and through hooks (`query.run`).
//! The engine itself is tested against real backends in `store/tests/query.rs`.
mod common;
use axum::Router;
use common::*;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use std::sync::Arc;
use stepwise::{
    control, dataplane,
    state::{AppState, ApplyOpts},
};

const AUTH: [(&str, &str); 1] = [("authorization", "Bearer ctl-token")];
const CTL_JSON: [(&str, &str); 2] = [
    ("authorization", "Bearer ctl-token"),
    ("content-type", "application/json"),
];

struct Env {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    st: Arc<AppState>,
    ctl: Router,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let mut c = base_config();
    c.database = match std::env::var("STEPWISE_TEST_POSTGRES_URL") {
        Ok(url) => parse(&format!(
            "enabled: true\ndb_type: postgres\nurl: '{url}'\nschema: 't_{}'\npool_size: 4",
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
    c.relations = parse(
        "orders:\n  items:    { from: orderItems, by: orderId }\n  customer: { from: customers, id: customerId }\norderItems:\n  product: { from: products, id: productId }",
    );
    let st = state(c);
    let _ = dataplane::router(st.clone());
    let db = st.db.clone().unwrap();
    for (c, id, doc) in [
        ("customers", "c1", json!({"id": "c1", "name": "Ann"})),
        ("customers", "c2", json!({"id": "c2", "name": "Bob"})),
        ("products", "p1", json!({"id": "p1", "name": "Pen"})),
        (
            "orders",
            "o1",
            json!({"id": "o1", "customerId": "c1", "total": 10, "status": "open"}),
        ),
        (
            "orders",
            "o2",
            json!({"id": "o2", "customerId": "c2", "total": 20, "status": "paid"}),
        ),
        (
            "orderItems",
            "i1",
            json!({"id": "i1", "orderId": "o1", "productId": "p1", "qty": 2}),
        ),
        (
            "orderItems",
            "i2",
            json!({"id": "i2", "orderId": "o1", "productId": "p1", "qty": 5}),
        ),
    ] {
        db.insert(c, id, &doc).await.unwrap();
    }
    Env {
        dir,
        ctl: control::router(st.clone()),
        st,
    }
}

async fn query(e: &Env, q: Value) -> Resp {
    send(
        &e.ctl,
        "POST",
        "/api/v1/db/query",
        &CTL_JSON,
        Some(&q.to_string()),
    )
    .await
}

#[tokio::test]
async fn the_control_plane_runs_queries_with_relations() {
    let e = env().await;
    let r = query(
        &e,
        json!({"from": "orders", "orderBy": "id", "total": true, "select": ["id", "total",
        {"customer": {"from": "customers", "id": "customerId", "select": ["name"]}},
        {"items": {"from": "orderItems", "by": "orderId", "select": ["qty"], "orderBy": "-qty"}}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(
        r.json(),
        json!({"total": 2, "data": [
        {"id": "o1", "total": 10, "customer": {"name": "Ann"}, "items": [{"qty": 5}, {"qty": 2}]},
        {"id": "o2", "total": 20, "customer": {"name": "Bob"}, "items": []}]})
    );

    // relations configured under `relations:` are used by name, including nested ones
    let r = query(&e, json!({"from": "orders", "id": "o1", "select": ["id", {"customer": {"rel": "customer", "select": ["name"]}},
        {"items": {"rel": "items", "select": ["qty", {"product": "product"}]}}]})).await;
    assert_eq!(
        r.json()["data"]["items"][0]["product"]["name"],
        "Pen",
        "{}",
        r.text
    );
    assert_eq!(
        query(
            &e,
            json!({"from": "orders", "where": {"total": {"gte": 15}}})
        )
        .await
        .json()["data"][0]["id"],
        "o2"
    );

    // errors say what is wrong
    let bad = query(&e, json!({"from": "orders", "slect": ["id"]})).await;
    assert_eq!(
        (bad.status.as_u16(), bad.json()["error"]["code"].clone()),
        (400, json!("invalid_query"))
    );
    assert!(bad.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown key `slect`"));
    let bad = query(&e, json!({"from": "orders", "select": [{"x": "nope"}]})).await;
    assert!(
        bad.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no relation `nope`"),
        "{}",
        bad.text
    );
    assert_eq!(
        send(&e.ctl, "POST", "/api/v1/db/query", &CTL_JSON, Some("{nope"))
            .await
            .json()["error"]["code"],
        "bad_json"
    );
    assert_eq!(
        send(&e.ctl, "POST", "/api/v1/db/query", &[], Some("{}"))
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn the_read_language_is_off_without_a_database() {
    let st = state(base_config());
    let ctl = control::router(st);
    let r = send(
        &ctl,
        "POST",
        "/api/v1/db/query",
        &CTL_JSON,
        Some(r#"{"from":"x"}"#),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"]["code"].clone()),
        (409, json!("database_disabled"))
    );
}

#[tokio::test]
async fn hooks_can_query_what_their_route_may_read() {
    let e = env().await;
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
    doc["routes"] = json!({ "POST /pets": { "db": { "grants": { "orders": "r", "orderItems": "r", "customers": "r" } } } });
    e.st.apply_json(doc, ApplyOpts::default()).await.unwrap();

    let ask = |q: Value| {
        let c = e.ctl.clone();
        async move {
            let path = format!(
                "/pets?mode=q-run&q={}",
                utf8_percent_encode(&q.to_string(), NON_ALPHANUMERIC)
            );
            let body = json!({"step": "pre_process", "request": {"method": "POST", "path": path, "body": {"name": "x"}}});
            let r = send(
                &c,
                "POST",
                "/api/v1/scripts/kitchen/versions/1/test",
                &CTL_JSON,
                Some(&body.to_string()),
            )
            .await;
            assert_eq!(r.status, 200, "{}", r.text);
            let j = r.json();
            assert_eq!(j["result"], "ok", "{j}");
            j["resulting"]["state"]["query"].clone()
        }
    };

    // a query across three collections the route is allowed to read
    let r = ask(json!({"from": "orders", "orderBy": "id", "select": ["id",
        {"customer": "customer"}, {"items": {"rel": "items", "select": ["qty"]}}]}))
    .await;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["val"]["data"][0]["customer"]["name"], "Ann");
    assert_eq!(r["val"]["data"][0]["items"].as_array().unwrap().len(), 2);

    // ... but not through a relation whose target is not granted (orderItems.product -> products)
    let r = ask(json!({"from": "orders", "id": "o1", "select": [{"items": {"rel": "items", "select": [{"product": "product"}]}}]})).await;
    assert_eq!(r["ok"], false, "{r}");
    assert!(
        r["err"].as_str().unwrap().contains("products"),
        "the message names the collection: {r}"
    );
    let r = ask(json!({"from": "secrets"})).await;
    assert!(r["err"].as_str().unwrap().contains("not granted"), "{r}");

    // malformed queries are explained, not swallowed
    let r = ask(json!({"from": "orders", "wher": {}})).await;
    assert!(
        r["err"].as_str().unwrap().contains("unknown key `wher`"),
        "{r}"
    );
}

#[tokio::test]
async fn relations_in_the_config_are_validated() {
    use stepwise::config::Config;
    let bad = |y: &str| {
        Config::from_yaml_str(&format!("control: {{enabled: false}}\n{y}"))
            .err()
            .map(|e| format!("{e:#}"))
    };
    assert!(bad("relations: {orders: {items: {from: orderItems}}}")
        .unwrap()
        .contains("exactly one of"));
    assert!(
        bad("relations: {orders: {items: {from: orderItems, by: orderId, id: x}}}")
            .unwrap()
            .contains("exactly one of")
    );
    assert!(
        bad("relations: {orders: {items: {from: 'a b', by: orderId}}}")
            .unwrap()
            .contains("target")
    );
    assert!(
        bad("relations: {orders: {items: {from: orderItems, by: orderId, typo: 1}}}").is_some(),
        "unknown keys are rejected"
    );
    assert!(bad("relations: {orders: {items: {from: orderItems, by: orderId}}}").is_none());
}
