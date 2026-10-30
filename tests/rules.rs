//! No-code data rules through the real data plane: authentication -> pipeline -> CRUD executor.
//! The rules' semantics are tested on both backends in `store/tests/rules.rs`; these tests cover the
//! wiring (auth identity, config validation, introspection). STEPWISE_TEST_POSTGRES_URL switches backend.
mod common;
use common::*;
use serde_json::{json, Value};
use std::sync::Arc;
use stepwise::{config::Config, control, dataplane, state::AppState};

const ANN: [(&str, &str); 2] = [
    ("authorization", "Bearer ann-key"),
    ("content-type", "application/json"),
];
const BOB: [(&str, &str); 2] = [
    ("authorization", "Bearer bob-key"),
    ("content-type", "application/json"),
];
const CTL: [(&str, &str); 1] = [("authorization", "Bearer ctl-token")];

fn rules_json() -> Value {
    json!({
        "owner": "ownerId",
        "timestamps": {"created": "createdAt", "updated": "updatedAt", "deleted": "deletedAt"},
        "readonly": ["status"],
        "defaults": {"status": "available"},
        "hide": ["internalNotes"],
        "soft_delete": "deleted",
        "audit": "audit"
    })
}

fn config(dir: &tempfile::TempDir, routes: Value) -> Config {
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
    c.auth = parse("enabled: true\nauth_type: token\ntoken: {keys: [{value: ann-key, subject: ann}, {value: bob-key, subject: bob}]}\nprotected_routes: ['/pets']");
    c.routes = serde_json::from_value(routes).unwrap();
    c
}

fn pets_routes(rules: Value) -> Value {
    let mut m = serde_json::Map::new();
    for r in [
        "GET /pets",
        "POST /pets",
        "GET /pets/{petId}",
        "PUT /pets/{petId}",
        "PATCH /pets/{petId}",
        "DELETE /pets/{petId}",
    ] {
        m.insert(r.into(), json!({ "data": rules }));
    }
    Value::Object(m)
}

fn start(c: Config) -> (Arc<AppState>, axum::Router, axum::Router) {
    let st = state(c);
    (
        st.clone(),
        dataplane::router(st.clone()),
        control::router(st),
    )
}

#[tokio::test]
async fn an_owner_scoped_soft_deleting_api_without_any_code() {
    let dir = tempfile::tempdir().unwrap();
    let (st, d, ctl) = start(config(&dir, pets_routes(rules_json())));

    let r = send(
        &d,
        "POST",
        "/pets",
        &ANN,
        Some(r#"{"name":"Rex","internalNotes":"secret","status":"sold","ownerId":"mallory"}"#),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text);
    let pet = r.json();
    assert_eq!(
        (
            pet["ownerId"].clone(),
            pet["status"].clone(),
            pet["deleted"].clone()
        ),
        (json!("ann"), json!("available"), json!(false)),
        "the owner comes from the token, not the body"
    );
    assert!(
        pet.get("internalNotes").is_none(),
        "hidden from the response"
    );
    assert!(pet["createdAt"].as_str().unwrap().ends_with('Z'));
    let id = pet["id"].as_i64().unwrap();

    // another user cannot see it; unauthenticated callers never reach it
    assert_eq!(
        send(&d, "GET", &format!("/pets/{id}"), &BOB, None)
            .await
            .status,
        404
    );
    assert_eq!(send(&d, "GET", "/pets", &BOB, None).await.json(), json!([]));
    assert_eq!(
        send(
            &d,
            "PUT",
            &format!("/pets/{id}"),
            &BOB,
            Some(r#"{"name":"stolen"}"#)
        )
        .await
        .status,
        404
    );
    assert_eq!(send(&d, "GET", "/pets", &[], None).await.status, 401);
    let mine = send(&d, "GET", "/pets", &ANN, None).await;
    assert_eq!(
        (
            mine.json().as_array().unwrap().len(),
            mine.header("x-total-count")
        ),
        (1, Some("1"))
    );

    // a replace cannot change the owner, the read-only status or createdAt
    let r = send(
        &d,
        "PUT",
        &format!("/pets/{id}"),
        &ANN,
        Some(r#"{"name":"Rex II","status":"sold","ownerId":"bob","createdAt":"X"}"#),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(
        (
            r.json()["name"].clone(),
            r.json()["status"].clone(),
            r.json()["ownerId"].clone(),
            r.json()["createdAt"].clone()
        ),
        (
            json!("Rex II"),
            json!("available"),
            json!("ann"),
            pet["createdAt"].clone()
        )
    );

    // DELETE is soft: gone to everyone, still stored, audited
    assert_eq!(
        send(&d, "DELETE", &format!("/pets/{id}"), &ANN, None)
            .await
            .status,
        204
    );
    assert_eq!(
        send(&d, "GET", &format!("/pets/{id}"), &ANN, None)
            .await
            .status,
        404
    );
    assert_eq!(send(&d, "GET", "/pets", &ANN, None).await.json(), json!([]));
    let stored = st
        .db
        .as_ref()
        .unwrap()
        .get("pets", &id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored["deleted"], true);
    assert_eq!(stored["internalNotes"], "secret", "hide is response-only");
    let events: Vec<String> = st
        .db
        .as_ref()
        .unwrap()
        .list(
            "audit",
            &stepwise::db::ListQuery {
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .iter()
        .map(|a| a["event"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        events,
        [
            "create POST /pets",
            "replace PUT /pets/{petId}",
            "delete DELETE /pets/{petId}"
        ]
    );

    // introspection
    let routes = send(&ctl, "GET", "/api/v1/routes", &CTL, None).await.json();
    let post = routes["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "POST /pets")
        .unwrap();
    assert_eq!(
        post["data_rules"],
        json!([
            "owner",
            "timestamps",
            "defaults",
            "readonly",
            "hide",
            "soft_delete",
            "audit"
        ])
    );
}

#[tokio::test]
async fn templated_effects_run_in_the_routes_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut rules = rules_json();
    rules["effects"] = json!([{ "op": "update", "collection": "products", "id": "${body.sku}", "inc": { "stock": "${-body.qty}" },
        "require": { "stock": { "gte": "${body.qty}" } }, "onFail": { "status": 409, "code": "out_of_stock", "message": "not enough stock" } }]);
    let (st, d, _) = start(config(&dir, json!({ "POST /pets": { "data": rules } })));
    st.db
        .as_ref()
        .unwrap()
        .insert("products", "sku-1", &json!({"id": "sku-1", "stock": 5}))
        .await
        .unwrap();
    let stock = || {
        let st = st.clone();
        async move {
            st.db
                .as_ref()
                .unwrap()
                .get("products", "sku-1")
                .await
                .unwrap()
                .unwrap()["stock"]
                .as_i64()
                .unwrap()
        }
    };

    assert_eq!(
        send(
            &d,
            "POST",
            "/pets",
            &ANN,
            Some(r#"{"name":"A","sku":"sku-1","qty":2}"#)
        )
        .await
        .status,
        201
    );
    assert_eq!(stock().await, 3);
    let r = send(
        &d,
        "POST",
        "/pets",
        &ANN,
        Some(r#"{"name":"B","sku":"sku-1","qty":9}"#),
    )
    .await;
    assert_eq!(
        (
            r.status.as_u16(),
            r.json()["error"]["code"].clone(),
            r.json()["error"]["message"].clone()
        ),
        (409, json!("out_of_stock"), json!("not enough stock"))
    );
    assert_eq!(
        (
            stock().await,
            st.db.as_ref().unwrap().count("pets", &[]).await.unwrap()
        ),
        (3, 1),
        "no pet, no stock change"
    );
    let r = send(&d, "POST", "/pets", &ANN, Some(r#"{"name":"C","qty":1}"#)).await;
    assert_eq!(r.status, 400);
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("body.sku"),
        "{}",
        r.text
    );
}

#[tokio::test]
async fn data_rules_are_checked_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let err = |c: Config| {
        format!(
            "{:#}",
            AppState::new(
                c,
                None,
                stepwise::hooks::HookRegistry::with_builtins(),
                None
            )
            .err()
            .expect("should be rejected")
        )
    };

    // rules need a route that talks to the database
    let e = err(config(
        &dir,
        json!({ "GET /health": { "data": { "hide": ["x"] } } }),
    ));
    assert!(e.contains("database-backed"), "{e}");
    // owner scoping needs an authenticated route
    let mut c = config(
        &dir,
        json!({ "POST /pets": { "data": { "owner": "ownerId" } } }),
    );
    c.auth = Default::default();
    let e = err(c);
    assert!(e.contains("authenticated"), "{e}");

    let bad = |y: &str| {
        Config::from_yaml_str(&format!("control: {{enabled: false}}\n{y}"))
            .err()
            .map(|e| format!("{e:#}"))
    };
    assert!(bad("routes: {'GET /pets': {data: {hide: [x]}}}")
        .unwrap()
        .contains("database.enabled"));
    let db = "database: {enabled: true}\n";
    assert!(bad(&format!(
        "{db}routes: {{'GET /pets': {{data: {{hide: [id]}}}}}}"
    ))
    .unwrap()
    .contains("`id`"));
    assert!(bad(&format!(
        "{db}routes: {{'GET /pets': {{data: {{owner: a.b}}}}}}"
    ))
    .unwrap()
    .contains("plain field"));
    assert!(bad(&format!(
        "{db}routes: {{'GET /pets': {{data: {{typo: 1}}}}}}"
    ))
    .unwrap()
    .contains("unknown field"));
    assert!(bad(&format!("{db}routes: {{'GET /pets': {{data: {{effects: [{{op: update, collection: '${{x}}'}}]}}}}}}")).unwrap().contains("collection"));
    assert!(bad(&format!(
        "{db}routes: {{'GET /pets': {{data: {{soft_delete: deleted}}}}}}"
    ))
    .is_none());
}
