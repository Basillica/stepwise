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

struct Env {
    #[allow(dead_code)] // keeps the temp directory (SQLite file) alive for the test's lifetime
    dir: tempfile::TempDir,
    st: Arc<AppState>,
    data: Router,
    ctl: Router,
}

fn env_with(tweak: impl FnOnce(&mut stepwise::config::Config)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let mut c = base_config();
    // Set STEPWISE_TEST_POSTGRES_URL to run this whole HTTP-level suite against PostgreSQL
    // (each Env gets its own schema).
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
        "enabled: true\ndata_dir: '{}'\nhook_timeout_ms: 1000",
        dir.path().join("w").display()
    ));
    tweak(&mut c);
    let st = state(c);
    Env {
        dir,
        data: dataplane::router(st.clone()),
        ctl: control::router(st.clone()),
        st,
    }
}

fn env() -> Env {
    env_with(|_| {})
}

async fn post(e: &Env, path: &str, body: Value) -> Resp {
    send(&e.data, "POST", path, &JSON, Some(&body.to_string())).await
}

#[tokio::test]
async fn full_lifecycle_of_a_resource() {
    let e = env();

    // create: ids come from a per-collection sequence; Location points at the new resource
    let r = post(&e, "/pets", json!({"name": "Rex", "tag": "dog"})).await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(r.json(), json!({"id": 1, "name": "Rex", "tag": "dog"}));
    assert_eq!(r.header("location"), Some("/pets/1"));
    assert_eq!(
        post(&e, "/pets", json!({"name": "Tom", "tag": "cat"}))
            .await
            .json()["id"],
        2
    );
    // a client-chosen id is honoured, and the sequence moves past it
    assert_eq!(
        post(&e, "/pets", json!({"id": 10, "name": "Ada", "tag": "dog"}))
            .await
            .json()["id"],
        10
    );
    assert_eq!(
        post(&e, "/pets", json!({"name": "Bo"})).await.json()["id"],
        11
    );
    let dup = post(&e, "/pets", json!({"id": 10, "name": "Dup"})).await;
    assert_eq!(
        (dup.status.as_u16(), dup.json()["error"]["code"].clone()),
        (409, json!("conflict"))
    );

    // read
    assert_eq!(
        send(&e.data, "GET", "/pets/2", &[], None).await.json()["name"],
        "Tom"
    );
    let missing = send(&e.data, "GET", "/pets/999", &[], None).await;
    assert_eq!(
        (
            missing.status.as_u16(),
            missing.json()["error"]["code"].clone()
        ),
        (404, json!("not_found"))
    );
    assert_eq!(
        send(&e.data, "GET", "/pets/abc", &[], None).await.status,
        400,
        "schema validation runs before the database"
    );

    // list: filters (declared params only), sort, pagination, total count
    let list = send(&e.data, "GET", "/pets", &[], None).await;
    assert_eq!(list.json().as_array().unwrap().len(), 4);
    assert_eq!(list.header("x-total-count"), Some("4"));
    let dogs = send(&e.data, "GET", "/pets?tag=dog", &[], None).await;
    assert_eq!(
        (
            dogs.json().as_array().unwrap().len(),
            dogs.header("x-total-count")
        ),
        (2, Some("2"))
    );
    let names = |r: Resp| {
        r.json()
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(send(&e.data, "GET", "/pets?sort=-name", &[], None).await),
        ["Tom", "Rex", "Bo", "Ada"]
    );
    let page = send(
        &e.data,
        "GET",
        "/pets?sort=name&limit=2&offset=1",
        &[],
        None,
    )
    .await;
    assert_eq!(
        (names(page.clone_resp()), page.header("x-total-count")),
        (vec!["Bo".to_string(), "Rex".to_string()], Some("4"))
    );
    assert_eq!(
        send(&e.data, "GET", "/pets?bogus=1", &[], None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        4,
        "undeclared params are not filters"
    );
    assert_eq!(
        send(&e.data, "GET", "/pets?limit=0", &[], None)
            .await
            .status,
        400
    );

    // replace: the URL's id wins over whatever the body says
    let r = send(&e.data, "PUT", "/pets/2", &JSON, Some(r#"{"name":"Max"}"#)).await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (200, json!({"id": 2, "name": "Max"}))
    );
    assert_eq!(
        send(
            &e.data,
            "PUT",
            "/pets/77",
            &JSON,
            Some(r#"{"name":"Ghost"}"#)
        )
        .await
        .status,
        404
    );

    // merge patch: null removes, other fields stay, ids are immutable
    let r = send(
        &e.data,
        "PATCH",
        "/pets/1",
        &JSON,
        Some(r#"{"tag":null,"status":"sold","id":99}"#),
    )
    .await;
    assert_eq!(
        r.json(),
        json!({"id": 1, "name": "Rex", "status": "sold"}),
        "{}",
        r.text
    );
    assert_eq!(
        send(&e.data, "PATCH", "/pets/77", &JSON, Some(r#"{"name":"x"}"#))
            .await
            .status,
        404
    );

    // delete
    let d = send(
        &e.data,
        "DELETE",
        "/pets/10",
        &[("authorization", "Bearer x")],
        None,
    )
    .await;
    assert_eq!((d.status.as_u16(), d.text.as_str()), (204, ""));
    assert_eq!(
        send(&e.data, "GET", "/pets/10", &[], None).await.status,
        404
    );
    assert_eq!(
        send(&e.data, "DELETE", "/pets/10", &[], None).await.status,
        404
    );
    assert_eq!(
        send(&e.data, "GET", "/pets", &[], None)
            .await
            .header("x-total-count"),
        Some("3")
    );
}

trait CloneResp {
    fn clone_resp(&self) -> Resp;
}
impl CloneResp for Resp {
    fn clone_resp(&self) -> Resp {
        Resp {
            status: self.status,
            headers: self.headers.clone(),
            text: self.text.clone(),
        }
    }
}

#[tokio::test]
async fn invalid_requests_never_reach_the_database() {
    let e = env();
    assert_eq!(
        post(&e, "/pets", json!({"tag": "no name"})).await.status,
        422
    );
    assert_eq!(
        send(&e.data, "POST", "/pets", &JSON, Some("{broken"))
            .await
            .status,
        400
    );
    assert_eq!(
        send(&e.data, "GET", "/pets", &[], None)
            .await
            .header("x-total-count"),
        Some("0")
    );
}

#[tokio::test]
async fn non_resources_and_opt_outs_keep_their_mock_responses() {
    let e = env_with(|c| {
        c.routes = parse("\"GET /pets/{petId}\": {crud: {operation: none}}");
    });
    assert_eq!(
        send(&e.data, "GET", "/health", &[], None).await.json(),
        json!({"status": "ok"}),
        "singleton GET is not a list"
    );
    assert_eq!(send(&e.data, "GET", "/broken", &[], None).await.status, 200);
    // opted out: generated mock (id 1 from `minimum`), even though no pet exists
    let r = send(&e.data, "GET", "/pets/5", &[], None).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["name"].clone()),
        (200, json!("string"))
    );
}

#[tokio::test]
async fn database_off_means_pure_mocks() {
    let st = state(base_config());
    let d = dataplane::router(st);
    let r = send(&d, "GET", "/pets", &[], None).await;
    assert_eq!(r.json()[0]["name"], "string");
    assert!(r.header("x-total-count").is_none());
}

#[tokio::test]
async fn crud_overrides_and_data_survive_restart() {
    let e = env_with(|c| {
        c.routes = parse("\"POST /pets\": {crud: {collection: animals}}\n\"GET /pets\": {crud: {collection: animals}}\n\"GET /pets/{petId}\": {crud: {collection: animals}}");
    });
    assert_eq!(post(&e, "/pets", json!({"name": "Rex"})).await.status, 201);
    let store = e.st.db.clone().unwrap();
    assert!(
        store.get("animals", "1").await.unwrap().is_some(),
        "stored under the configured collection"
    );
    assert!(store.get("pets", "1").await.unwrap().is_none());

    // a brand new server on the same file sees the data
    let cfg = (*e.st.config()).clone();
    let st2 = state(cfg);
    let d2 = dataplane::router(st2);
    assert_eq!(
        send(&d2, "GET", "/pets/1", &[], None).await.json()["name"],
        "Rex"
    );
}

#[tokio::test]
async fn config_validation_for_the_database() {
    let bad = |yaml: &str| {
        Config::from_yaml_str(&format!("control: {{enabled: false}}\n{yaml}"))
            .err()
            .map(|e| format!("{e:#}"))
    };
    use stepwise::config::Config;
    assert!(bad("database: {enabled: true, db_type: mysql}")
        .unwrap()
        .contains("not supported yet"));
    assert!(
        bad("database: {enabled: true, db_type: postgres}")
            .unwrap()
            .contains("postgres://"),
        "postgres needs a url"
    );
    assert!(
        bad("database: {enabled: true, db_type: postgres, url: 'mysql://x'}")
            .unwrap()
            .contains("postgres://")
    );
    assert!(bad("database: {enabled: true, db_type: postgres, url: 'postgres://u@h/d', schema: 'Bad Schema'}").unwrap().contains("schema"));
    assert!(bad(
        "database: {enabled: true, db_type: postgres, url: 'postgres://u:p@h/d', schema: app}"
    )
    .is_none());
    assert!(bad("routes: {'GET /x': {db: {grants: {a: r}}}}")
        .unwrap()
        .contains("database.enabled"));
    assert!(
        bad("database: {enabled: true}\nroutes: {'GET /x': {db: {grants: {a: rx}}}}")
            .unwrap()
            .contains("must be one of")
    );
    assert!(bad("database: {enabled: true}").is_none());
}

#[tokio::test]
async fn hooks_hold_the_business_logic_the_database_holds_the_data() {
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
    doc["routes"] = json!({
        "POST /pets": { "steps": { "pre_process": { "before": ["wasm:kitchen@1"] } } },
        "GET /pets": { "steps": { "post_process": { "after": ["wasm:kitchen@1"] } } },
    });
    e.st.apply_json(doc, ApplyOpts::default()).await.unwrap();

    // pre_process normalizes the name *before* it is validated and stored
    let r = send(
        &e.data,
        "POST",
        "/pets?mode=normalize",
        &JSON,
        Some(r#"{"name":"  rex the dog "}"#),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["name"].clone()),
        (201, json!("Rex The Dog")),
        "{}",
        r.text
    );
    // the *stored* document is the normalized one
    assert_eq!(
        e.st.db
            .as_ref()
            .unwrap()
            .get("pets", "1")
            .await
            .unwrap()
            .unwrap()["name"],
        "Rex The Dog"
    );

    // a hook's business rule can refuse before anything is written
    let rej = send(
        &e.data,
        "POST",
        "/pets?mode=reject",
        &JSON,
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

    // post_process decorates what the database returned
    let l = send(&e.data, "GET", "/pets?mode=post", &[], None).await;
    assert_eq!(l.json()[0]["decoratedBy"], "post-hook");
    assert_eq!(l.json()[0]["name"], "Rex The Dog");
    assert_eq!(l.header("x-hook"), Some("ok"));
    assert_eq!(
        l.header("x-total-count"),
        Some("1"),
        "headers from execute survive the hook (it re-sends them)"
    );
}

#[tokio::test]
async fn hook_database_access_is_governed_by_grants() {
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
    for (n, t) in [("Rex", "dog"), ("Tom", "cat"), ("Ada", "dog")] {
        assert_eq!(
            post(&e, "/pets", json!({"name": n, "tag": t})).await.status,
            201
        );
    }
    let mut doc = serde_json::to_value(e.st.config().redacted()).unwrap();
    doc["routes"] = json!({
        "POST /pets": { "steps": { "pre_process": { "before": ["wasm:kitchen@1"] } }, "db": { "grants": { "audit": "rw" } } },
        "GET /pets": { "steps": { "pre_process": { "before": ["wasm:kitchen@1"] } } },
        "GET /health": { "steps": { "pre_process": { "before": ["wasm:kitchen@1"] } } },
    });
    e.st.apply_json(doc, ApplyOpts::default()).await.unwrap();

    // run a hook version in the sandbox; returns what the guest saw from the database
    let probe = |method: &'static str, path: String, db: Option<&'static str>| {
        let c = e.ctl.clone();
        async move {
            let mut body = json!({ "step": "pre_process", "request": { "method": method, "path": path, "body": {"name": "x"} } });
            if let Some(d) = db {
                body["db"] = json!(d);
            }
            let r = send(
                &c,
                "POST",
                "/api/v1/scripts/kitchen/versions/1/test",
                &[
                    ("authorization", "Bearer ctl-token"),
                    ("content-type", "application/json"),
                ],
                Some(&body.to_string()),
            )
            .await;
            assert_eq!(r.status, 200, "{}", r.text);
            let j = r.json();
            assert_eq!(j["result"], "ok", "{j}");
            j["resulting"]["state"]["db"].clone()
        }
    };

    // the route's own CRUD collection is readable without any grant
    assert_eq!(
        probe("GET", "/pets?mode=db-count".into(), None).await,
        json!({"ok": true, "val": 3})
    );
    let got = probe("GET", "/pets?mode=db-get&id=1".into(), None).await;
    assert_eq!(
        serde_json::from_str::<Value>(got["val"].as_str().unwrap()).unwrap()["name"],
        "Rex"
    );
    let found = probe("GET", "/pets?mode=db-find".into(), None).await;
    assert_eq!(
        found["val"].as_array().unwrap().len(),
        2,
        "filter on tag=dog: {found}"
    );
    assert_eq!(
        probe("GET", "/pets?mode=db-get&id=404".into(), None).await["val"],
        Value::Null,
        "missing doc is none, not an error"
    );

    // collections that are not granted are refused, with a message that says so
    let denied = probe("GET", "/pets?mode=db-denied".into(), None).await;
    assert_eq!(denied["ok"], false);
    assert!(
        denied["err"].as_str().unwrap().contains("not granted"),
        "{denied}"
    );
    let no_audit = probe("GET", "/pets?mode=db-insert&id=t1".into(), None).await;
    assert!(
        no_audit["err"].as_str().unwrap().contains("not granted"),
        "GET /pets has no grant for `audit`: {no_audit}"
    );

    // POST /pets *is* granted audit:rw, but test runs are read-only unless asked otherwise
    let ro = probe("POST", "/pets?mode=db-insert&id=t1".into(), None).await;
    assert!(ro["err"].as_str().unwrap().contains("read-only"), "{ro}");
    let store = e.st.db.clone().unwrap();
    assert!(
        store.get("audit", "t1").await.unwrap().is_none(),
        "a read-only test wrote nothing"
    );
    let live = probe("POST", "/pets?mode=db-insert&id=t1".into(), Some("live")).await;
    assert_eq!(live, json!({"ok": true, "val": true}));
    assert_eq!(
        store.get("audit", "t1").await.unwrap().unwrap(),
        json!({"by": "POST /pets", "step": "pre_process"})
    );
    assert_eq!(
        probe("POST", "/pets?mode=db-insert&id=t1".into(), Some("live")).await["val"],
        false,
        "id already exists"
    );

    // writes to the route's own collection work in live mode; deletes too
    assert_eq!(
        probe("GET", "/pets?mode=db-merge&id=2".into(), Some("live")).await["ok"],
        true
    );
    assert_eq!(store.get("pets", "2").await.unwrap().unwrap()["seen"], true);
    assert_eq!(
        probe("GET", "/pets?mode=db-delete&id=2".into(), Some("live")).await["val"],
        true
    );
    assert!(store.get("pets", "2").await.unwrap().is_none());

    // no database / no grants at all
    let none = probe("GET", "/pets?mode=db-count".into(), Some("none")).await;
    assert!(
        none["err"].as_str().unwrap().contains("not available"),
        "{none}"
    );
    let bare = probe("GET", "/health?mode=db-count".into(), Some("live")).await;
    assert!(
        bare["err"].as_str().unwrap().contains("not available"),
        "a route with no grants gets no database: {bare}"
    );
    let bad = send(
        &e.ctl,
        "POST",
        "/api/v1/scripts/kitchen/versions/1/test",
        &[("authorization", "Bearer ctl-token")],
        Some(r#"{"request":{"path":"/pets"},"db":"yolo"}"#),
    )
    .await;
    assert_eq!(bad.status, 400);

    // and for real traffic: the hook's audit write happens alongside the CRUD create
    let r = send(
        &e.data,
        "POST",
        "/pets?mode=db-insert&id=req1",
        &JSON,
        Some(r#"{"name":"Zed"}"#),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(
        store.get("audit", "req1").await.unwrap().unwrap()["step"],
        "pre_process"
    );
    assert_eq!(
        store
            .count("pets", &[("name".into(), "Zed".into())])
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn control_plane_can_seed_browse_and_reset_data() {
    let e = env();
    let c = |m: &'static str, p: &str, b: Option<&str>| {
        let (ctl, p, b) = (e.ctl.clone(), p.to_string(), b.map(String::from));
        async move {
            send(
                &ctl,
                m,
                &format!("/api/v1{p}"),
                &[("authorization", "Bearer ctl-token")],
                b.as_deref(),
            )
            .await
        }
    };
    assert_eq!(c("GET", "/ready", None).await.json()["database"], "ok");

    // seeding follows the API's own id rules (integer sequence, because Pet.id is an integer)
    let r = c(
        "POST",
        "/db/collections/pets/docs",
        Some(r#"[{"name":"A","tag":"dog"},{"id":7,"name":"B","tag":"cat"}]"#),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(
        (r.json()["inserted"].clone(), r.json()["ids"].clone()),
        (json!(2), json!(["1", "7"]))
    );
    let again = c(
        "POST",
        "/db/collections/pets/docs",
        Some(r#"{"id":7,"name":"B2"}"#),
    )
    .await
    .json();
    assert_eq!(
        (again["inserted"].clone(), again["skipped_existing"].clone()),
        (json!(0), json!(1)),
        "seeding is repeatable"
    );

    // ...and it is the same data the API serves
    assert_eq!(
        send(&e.data, "GET", "/pets/7", &[], None).await.json()["name"],
        "B"
    );
    assert_eq!(
        post(&e, "/pets", json!({"name": "C"})).await.json()["id"],
        8,
        "sequence moved past the seeded id"
    );

    // browse
    let listing = c("GET", "/db/collections", None).await.json();
    assert_eq!(
        listing["collections"],
        json!([{"name": "pets", "count": 3}])
    );
    let dogs = c("GET", "/db/collections/pets/docs?tag=dog", None)
        .await
        .json();
    assert_eq!(
        (dogs["total"].clone(), dogs["docs"][0]["name"].clone()),
        (json!(1), json!("A"))
    );
    assert_eq!(
        c("GET", "/db/collections/pets/docs?sort=-name&limit=1", None)
            .await
            .json()["docs"][0]["name"],
        "C"
    );
    assert_eq!(
        c("GET", "/db/collections/pets/docs?a%27b=x", None)
            .await
            .status,
        400,
        "hostile field names are refused"
    );
    assert_eq!(
        c("GET", "/db/collections/bad%20name/docs", None)
            .await
            .json()["error"]["code"],
        "bad_name"
    );
    assert_eq!(
        c("POST", "/db/collections/pets/docs", Some("42"))
            .await
            .status,
        400
    );

    // reset: data gone and the sequence starts over
    assert_eq!(
        c("DELETE", "/db/collections/pets", None).await.json()["deleted"],
        3
    );
    assert_eq!(
        send(&e.data, "GET", "/pets", &[], None).await.json(),
        json!([])
    );
    assert_eq!(
        post(&e, "/pets", json!({"name": "Fresh"})).await.json()["id"],
        1
    );
}

#[tokio::test]
async fn control_db_endpoints_report_a_disabled_database() {
    let st = state(base_config());
    let ctl = control::router(st);
    let r = send(&ctl, "GET", "/api/v1/db/collections", &AUTH, None).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"]["code"].clone()),
        (409, json!("database_disabled"))
    );
    assert_eq!(
        send(&ctl, "GET", "/api/v1/ready", &[], None).await.json()["database"],
        "disabled"
    );
}
