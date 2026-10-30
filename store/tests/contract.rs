//! Behavioural contracts, run against every `DocStore` backend: plain CRUD semantics, and the
//! transactional effects engine (atomicity, rollback, versions, and concurrency properties).
//!
//! * SQLite always runs.
//! * PostgreSQL runs when `STEPWISE_TEST_POSTGRES_URL` is set, e.g.
//!       STEPWISE_TEST_POSTGRES_URL=postgres://postgres:pw@127.0.0.1:5432/postgres \
//!       cargo test -p stepwise-store          (and `--test db` in the server crate for HTTP-level)
//!   (`scripts/test-postgres.sh` starts a throwaway container and does exactly that).
//!   Each run uses its own schema (`t_<random>`) and drops it afterwards.
//! * TLS: set STEPWISE_TEST_POSTGRES_TLS_URL (a `sslmode=require` URL of a TLS-enabled server) and
//!   STEPWISE_TEST_POSTGRES_CA (PEM of the CA that signed its certificate).
use std::sync::Arc;

use serde_json::{json, Value};
use stepwise_store::{
    ApplyError, DocStore, Effect, ListQuery, OpKind, PostgresStore, ResultStatus, SqliteStore,
};

fn q(
    filters: &[(&str, &str)],
    sort: Option<(&str, bool)>,
    limit: usize,
    offset: usize,
) -> ListQuery {
    ListQuery {
        filters: filters
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        in_filters: vec![],
        ne_filters: vec![],
        sort: sort.map(|(f, d)| (f.to_string(), d)),
        limit,
        offset,
    }
}

fn names(v: Vec<Value>) -> Vec<String> {
    v.iter()
        .map(|d| d["name"].as_str().unwrap_or("<none>").to_string())
        .collect()
}

async fn contract(s: Arc<dyn DocStore>) {
    s.ping().await.expect("ping");

    // ---- insert / get / isolation / replace / delete
    assert!(s
        .insert(
            "pets",
            "1",
            &json!({"id": 1, "name": "Rex", "tag": "dog", "meta": {"a": 1, "b": 2}})
        )
        .await
        .unwrap());
    assert!(
        !s.insert("pets", "1", &json!({"id": 1})).await.unwrap(),
        "duplicate id is refused, not overwritten"
    );
    assert_eq!(s.get("pets", "1").await.unwrap().unwrap()["name"], "Rex");
    assert!(s.get("pets", "404").await.unwrap().is_none());
    assert!(
        s.get("other", "1").await.unwrap().is_none(),
        "collections are isolated"
    );
    assert!(s
        .replace("pets", "1", &json!({"id": 1, "name": "Max"}))
        .await
        .unwrap());
    assert_eq!(
        s.get("pets", "1").await.unwrap().unwrap(),
        json!({"id": 1, "name": "Max"}),
        "replace overwrites the whole doc"
    );
    assert!(!s.replace("pets", "9", &json!({})).await.unwrap());
    assert!(s.delete("pets", "1").await.unwrap());
    assert!(!s.delete("pets", "1").await.unwrap());

    // ---- RFC 7396 merge patch
    s.insert(
        "m",
        "1",
        &json!({"a": "b", "keep": true, "meta": {"x": 1, "y": 2}, "arr": [1, 2]}),
    )
    .await
    .unwrap();
    let merged = s
        .merge(
            "m",
            "1",
            &json!({"a": null, "meta": {"y": null, "z": 3}, "arr": [9], "new": "v"}),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        merged,
        json!({"keep": true, "meta": {"x": 1, "z": 3}, "arr": [9], "new": "v"})
    );
    assert_eq!(
        s.get("m", "1").await.unwrap().unwrap(),
        merged,
        "merge result is what is stored"
    );
    assert!(s
        .merge("m", "nope", &json!({"a": 1}))
        .await
        .unwrap()
        .is_none());

    // ---- list: filters on string / number / bool / nested, sort, pagination, count
    for (i, (n, kind, ok)) in [
        ("b", "dog", true),
        ("a", "cat", false),
        ("c", "dog", true),
        ("d", "dog", false),
    ]
    .iter()
    .enumerate()
    {
        s.insert(
            "l",
            &i.to_string(),
            &json!({"name": n, "kind": kind, "ok": ok, "n": i, "owner": {"name": "ann"}}),
        )
        .await
        .unwrap();
    }
    s.insert(
        "l",
        "no-name",
        &json!({"kind": "bird", "ok": true, "n": 9, "x": null}),
    )
    .await
    .unwrap();
    assert_eq!(
        names(s.list("l", &q(&[], None, 10, 0)).await.unwrap()),
        ["b", "a", "c", "d", "<none>"],
        "insertion order by default"
    );
    assert_eq!(
        names(
            s.list("l", &q(&[("kind", "dog")], Some(("name", false)), 10, 0))
                .await
                .unwrap()
        ),
        ["b", "c", "d"]
    );
    assert_eq!(
        names(
            s.list(
                "l",
                &q(
                    &[("kind", "dog"), ("ok", "true")],
                    Some(("name", true)),
                    10,
                    0
                )
            )
            .await
            .unwrap()
        ),
        ["c", "b"],
        "bool filter"
    );
    assert_eq!(
        names(s.list("l", &q(&[("n", "2")], None, 10, 0)).await.unwrap()),
        ["c"],
        "numeric filter"
    );
    assert_eq!(
        s.list("l", &q(&[("owner.name", "ann")], None, 10, 0))
            .await
            .unwrap()
            .len(),
        4,
        "nested field"
    );
    assert_eq!(
        s.list("l", &q(&[("x", "null")], None, 10, 0))
            .await
            .unwrap()
            .len(),
        0,
        "JSON null never matches a filter"
    );
    assert_eq!(
        s.list("l", &q(&[("missing", "x")], None, 10, 0))
            .await
            .unwrap()
            .len(),
        0
    );
    // missing sort values: first ascending, last descending (both backends agree)
    assert_eq!(
        names(
            s.list("l", &q(&[], Some(("name", false)), 10, 0))
                .await
                .unwrap()
        ),
        ["<none>", "a", "b", "c", "d"]
    );
    assert_eq!(
        names(
            s.list("l", &q(&[], Some(("name", true)), 10, 0))
                .await
                .unwrap()
        ),
        ["d", "c", "b", "a", "<none>"]
    );
    assert_eq!(
        names(
            s.list("l", &q(&[], Some(("name", false)), 2, 2))
                .await
                .unwrap()
        ),
        ["b", "c"],
        "limit/offset"
    );
    assert_eq!(
        s.count("l", &[("kind".into(), "dog".into())])
            .await
            .unwrap(),
        3
    );
    assert_eq!(s.count("l", &[]).await.unwrap(), 5);
    assert_eq!(s.count("empty", &[]).await.unwrap(), 0);

    // ---- IN filters, rows with ids, get_many
    let inq = |field: &str, vals: &[&str]| ListQuery {
        in_filters: vec![(field.into(), vals.iter().map(|v| v.to_string()).collect())],
        limit: 50,
        ..Default::default()
    };
    let rows = s
        .list_rows("l", &inq("kind", &["cat", "bird"]))
        .await
        .unwrap();
    let mut ids: Vec<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["1", "no-name"], "rows carry their row id");
    assert_eq!(
        s.list_rows("l", &inq("n", &["2", "9"]))
            .await
            .unwrap()
            .len(),
        2,
        "numbers compare as text, like equality filters"
    );
    assert_eq!(
        s.list_rows("l", &inq("ok", &["false"]))
            .await
            .unwrap()
            .len(),
        2,
        "booleans too"
    );
    assert_eq!(
        s.list_rows("l", &inq("kind", &[])).await.unwrap().len(),
        0,
        "an empty IN list matches nothing"
    );
    assert!(
        s.list_rows("l", &inq("kind') --", &["x"])).await.is_err(),
        "IN field names are validated"
    );
    let both = ListQuery {
        filters: vec![("kind".into(), "dog".into())],
        in_filters: vec![("n".into(), vec!["0".into(), "3".into()])],
        limit: 50,
        ..Default::default()
    };
    assert_eq!(
        s.list_rows("l", &both).await.unwrap().len(),
        2,
        "equality and IN filters combine"
    );
    let mut got: Vec<String> = s
        .get_many("l", &["0".into(), "3".into(), "nope".into()])
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    got.sort();
    assert_eq!(got, ["0", "3"], "missing ids are simply absent");
    assert!(s.get_many("l", &[]).await.unwrap().is_empty());
    assert!(
        s.get_many("other-collection", &["0".into()])
            .await
            .unwrap()
            .is_empty(),
        "get_many respects the collection"
    );

    // ---- NOT-equal filters: documents without the field match too (soft-delete style)
    let nq = |field: &str, v: &str| ListQuery {
        ne_filters: vec![(field.into(), v.into())],
        limit: 50,
        ..Default::default()
    };
    assert_eq!(
        s.list_rows("l", &nq("ok", "true")).await.unwrap().len(),
        2,
        "`ok` is false on two rows; the others are true"
    );
    assert_eq!(
        s.list_rows("l", &nq("missing", "true"))
            .await
            .unwrap()
            .len(),
        5,
        "a field absent everywhere is never equal to the value"
    );
    assert_eq!(
        s.list_rows("l", &nq("kind", "dog")).await.unwrap().len(),
        2,
        "cat and bird"
    );
    assert_eq!(
        s.count_matching("l", &nq("ok", "true")).await.unwrap(),
        2,
        "count honours the same filters"
    );
    let mixed = ListQuery {
        filters: vec![("kind".into(), "dog".into())],
        ne_filters: vec![("ok".into(), "true".into())],
        limit: 50,
        ..Default::default()
    };
    assert_eq!(
        s.list_rows("l", &mixed).await.unwrap().len(),
        1,
        "combines with equality filters"
    );

    // ---- hostile input is rejected or inert
    assert!(s
        .list("l", &q(&[("name') OR 1=1 --", "x")], None, 10, 0))
        .await
        .is_err());
    assert!(s
        .list(
            "l",
            &q(&[], Some(("name; DROP TABLE stepwise_docs", false)), 10, 0)
        )
        .await
        .is_err());
    assert_eq!(
        s.list("l", &q(&[("name", "' OR '1'='1")], None, 10, 0))
            .await
            .unwrap()
            .len(),
        0,
        "values are bound parameters"
    );
    assert!(s.count("l", &[("a b".into(), "x".into())]).await.is_err());

    // ---- awkward ids and content
    let odd = "ключ 🦀 O'Brien; \"quoted\" \\ %_";
    assert!(s
        .insert(
            "odd",
            odd,
            &json!({"emoji": "🦀", "quote": "it's \"fine\"", "nul_escape": "\\u0000 is just text"})
        )
        .await
        .unwrap());
    assert_eq!(s.get("odd", odd).await.unwrap().unwrap()["emoji"], "🦀");
    let big = "x".repeat(200_000);
    s.insert("odd", "big", &json!({"blob": big})).await.unwrap();
    assert_eq!(
        s.get("odd", "big").await.unwrap().unwrap()["blob"]
            .as_str()
            .unwrap()
            .len(),
        200_000
    );
    assert!(
        s.insert("odd", "", &json!({})).await.is_err(),
        "an empty id is rejected, not stored"
    );
    assert!(
        s.insert("odd", &"i".repeat(257), &json!({})).await.is_err(),
        "ids are limited to 256 characters"
    );

    // ---- sequences
    let (a, b, c) = (
        s.next_sequence("seq-a").await.unwrap(),
        s.next_sequence("seq-a").await.unwrap(),
        s.next_sequence("seq-b").await.unwrap(),
    );
    assert_eq!((a, b, c), (1, 2, 1), "independent per collection");
    s.bump_sequence("seq-a", 10).await.unwrap();
    s.bump_sequence("seq-a", 3).await.unwrap();
    assert_eq!(
        s.next_sequence("seq-a").await.unwrap(),
        11,
        "bump never moves backwards"
    );

    // ---- collections / clear
    let cols: Vec<String> = s
        .collections()
        .await
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(
        cols,
        ["l", "m", "odd"],
        "sorted, only collections that hold documents"
    );
    assert_eq!(s.clear("l").await.unwrap(), 5);
    assert_eq!(s.count("l", &[]).await.unwrap(), 0);
    assert_eq!(
        s.clear("seq-a").await.unwrap(),
        0,
        "clearing resets the sequence even with no documents"
    );
    assert_eq!(s.next_sequence("seq-a").await.unwrap(), 1);

    // ---- concurrency: unique sequence ids under load, and no lost merge updates
    let mut tasks = Vec::new();
    for i in 0..40 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let id = s.next_sequence("conc").await.unwrap();
            assert!(s
                .insert("conc", &id.to_string(), &json!({"i": i}))
                .await
                .unwrap());
            id
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.unwrap());
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 40, "every concurrent caller got a distinct id");
    assert_eq!(s.count("conc", &[]).await.unwrap(), 40);

    s.insert("locks", "doc", &json!({})).await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..30 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            s.merge("locks", "doc", &json!({ format!("k{i}"): i }))
                .await
                .unwrap()
                .unwrap()
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let doc = s.get("locks", "doc").await.unwrap().unwrap();
    assert_eq!(
        doc.as_object().unwrap().len(),
        30,
        "concurrent merges must not overwrite each other: {doc}"
    );
}

#[tokio::test]
async fn sqlite_satisfies_the_store_contract() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("c.db").to_str().unwrap(), 4, 5000).unwrap();
    contract(store.clone()).await;
    effects_contract(store.clone()).await;
    // and survives reopening
    let again = SqliteStore::open(dir.path().join("c.db").to_str().unwrap(), 2, 5000).unwrap();
    assert!(again.get("m", "1").await.unwrap().is_some());
}

#[tokio::test]
async fn postgres_satisfies_the_store_contract() {
    let Ok(url) = std::env::var("STEPWISE_TEST_POSTGRES_URL") else {
        eprintln!("SKIP: set STEPWISE_TEST_POSTGRES_URL to run the PostgreSQL contract");
        return;
    };
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let store = PostgresStore::new(&url, &schema, 6, None).expect("pool");
    contract(store.clone()).await;
    effects_contract(store.clone()).await;

    // a second instance on the same schema sees the data (shared database, not per-process state)
    let other = PostgresStore::new(&url, &schema, 2, None).unwrap();
    assert_eq!(other.get("m", "1").await.unwrap().unwrap()["new"], "v");
    // a different schema is a different namespace
    let schema2 = format!("t_{}", uuid::Uuid::new_v4().simple());
    let isolated = PostgresStore::new(&url, &schema2, 2, None).unwrap();
    assert!(isolated.get("m", "1").await.unwrap().is_none());
    assert!(isolated.collections().await.unwrap().is_empty());

    store.drop_schema_cascade().await.unwrap();
    isolated.drop_schema_cascade().await.unwrap();
}

/// Needs no server: bad configuration and unreachable servers must fail fast and clearly.
#[tokio::test]
async fn postgres_configuration_errors_are_clear() {
    assert!(PostgresStore::new("not a url", "public", 2, None).is_err());
    let bad_schema = PostgresStore::new("postgres://u:p@127.0.0.1/db", "Bad-Schema; DROP", 2, None);
    assert!(bad_schema
        .err()
        .unwrap()
        .to_string()
        .contains("database.schema"));
    // nothing listens on port 1: connection refused must surface as an error, not hang
    let s = PostgresStore::new(
        "postgres://u:p@127.0.0.1:1/db?sslmode=disable&connect_timeout=2",
        "public",
        2,
        None,
    )
    .unwrap();
    let t0 = std::time::Instant::now();
    let err = s.ping().await.unwrap_err();
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(20),
        "failed fast"
    );
    assert!(
        format!("{err:#}").to_lowercase().contains("connect"),
        "{err:#}"
    );
    // TLS requested but the CA file does not exist
    let r = PostgresStore::new(
        "postgres://u:p@127.0.0.1/db?sslmode=require",
        "public",
        2,
        Some("/nonexistent-ca.pem"),
    );
    assert!(r.err().unwrap().to_string().contains("tls_ca_path"));
}

#[tokio::test]
async fn postgres_tls_verifies_the_server_certificate() {
    let (Ok(url), Ok(ca)) = (
        std::env::var("STEPWISE_TEST_POSTGRES_TLS_URL"),
        std::env::var("STEPWISE_TEST_POSTGRES_CA"),
    ) else {
        eprintln!("SKIP: set STEPWISE_TEST_POSTGRES_TLS_URL and STEPWISE_TEST_POSTGRES_CA to run the TLS check");
        return;
    };
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let good = PostgresStore::new(&url, &schema, 2, Some(&ca)).unwrap();
    good.ping().await.expect("trusted CA: connection works");
    good.drop_schema_cascade().await.unwrap();

    // a CA that did not sign the server's certificate must be refused
    let dir = tempfile::tempdir().unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let rogue = params.self_signed(&key).unwrap();
    std::fs::write(dir.path().join("rogue.pem"), rogue.pem()).unwrap();
    let bad = PostgresStore::new(
        &url,
        &schema,
        2,
        Some(dir.path().join("rogue.pem").to_str().unwrap()),
    )
    .unwrap();
    assert!(
        bad.ping().await.is_err(),
        "an untrusted CA must fail certificate verification"
    );
}

// ====================================================================================
// Effects: transactional plans
// ====================================================================================

fn fx(v: Value) -> Effect {
    serde_json::from_value(v).expect("effect parses")
}

fn failure_code(e: &ApplyError) -> Option<String> {
    match e {
        ApplyError::Precondition {
            on_fail: Some(f), ..
        } => Some(f.code.clone()),
        _ => None,
    }
}

async fn effects_contract(s: Arc<dyn DocStore>) {
    // ---- versions: 1 on insert, +1 on every write
    let r = s
        .apply(&[Effect::insert("ver", "1", json!({"id": "1", "n": 1}))])
        .await
        .unwrap();
    assert_eq!(
        (r[0].status, r[0].version),
        (ResultStatus::Inserted, Some(1))
    );
    assert_eq!(s.get_versioned("ver", "1").await.unwrap().unwrap().1, 1);
    let r = s
        .apply(&[fx(
            json!({"op": "update", "collection": "ver", "id": "1", "inc": {"n": 1}}),
        )])
        .await
        .unwrap();
    assert_eq!(
        (r[0].status, r[0].version, r[0].doc.clone()),
        (
            ResultStatus::Updated,
            Some(2),
            Some(json!({"id": "1", "n": 2}))
        )
    );
    s.replace("ver", "1", &json!({"id": "1", "n": 9}))
        .await
        .unwrap();
    s.merge("ver", "1", &json!({"extra": true})).await.unwrap();
    assert_eq!(
        s.get_versioned("ver", "1").await.unwrap().unwrap(),
        (json!({"id": "1", "n": 9, "extra": true}), 4),
        "replace and merge each bump the version"
    );
    assert!(
        s.apply(&[]).await.unwrap().is_empty(),
        "an empty plan is a no-op"
    );

    // ---- expectVersion
    let stale = fx(
        json!({"op": "update", "collection": "ver", "id": "1", "set": {"n": 0}, "expectVersion": 3}),
    );
    assert!(matches!(
        s.apply(&[stale]).await,
        Err(ApplyError::VersionMismatch {
            expected: 3,
            actual: 4,
            ..
        })
    ));
    let good = fx(
        json!({"op": "update", "collection": "ver", "id": "1", "set": {"n": 0}, "expectVersion": 4}),
    );
    assert_eq!(s.apply(&[good]).await.unwrap()[0].version, Some(5));

    // ---- a multi-collection plan is all-or-nothing
    s.insert("products", "sku-1", &json!({"id": "sku-1", "stock": 5}))
        .await
        .unwrap();
    let order = |id: &str, qty: i64, need: i64| {
        vec![
            Effect::insert("orders", id, json!({"id": id, "sku": "sku-1", "qty": qty})),
            fx(
                json!({"op": "update", "collection": "products", "id": "sku-1", "inc": {"stock": -qty},
                      "require": {"stock": {"gte": need}}, "onFail": {"status": 409, "code": "out_of_stock"}}),
            ),
            fx(
                json!({"op": "insert", "collection": "audit", "doc": {"event": "order", "order": id}}),
            ),
        ]
    };
    let r = s.apply(&order("o1", 2, 2)).await.unwrap();
    assert_eq!(r.len(), 3);
    assert_eq!(
        s.get("products", "sku-1").await.unwrap().unwrap()["stock"],
        3
    );
    assert!(s.get("orders", "o1").await.unwrap().is_some());
    let audit_id = r[2].id.clone();
    assert_eq!(
        s.get("audit", &audit_id).await.unwrap().unwrap()["id"],
        audit_id.as_str(),
        "an insert without an id gets a uuid, also written into the doc"
    );

    let before_audit = s.count("audit", &[]).await.unwrap();
    let e = s.apply(&order("o2", 9, 9)).await.unwrap_err();
    assert!(
        matches!(e, ApplyError::Precondition { index: 1, .. }),
        "{e}"
    );
    assert_eq!(failure_code(&e).as_deref(), Some("out_of_stock"));
    assert!(
        s.get("orders", "o2").await.unwrap().is_none(),
        "the order inserted *before* the failing effect was rolled back"
    );
    assert_eq!(
        s.get("products", "sku-1").await.unwrap().unwrap()["stock"],
        3,
        "stock untouched"
    );
    assert_eq!(
        s.count("audit", &[]).await.unwrap(),
        before_audit,
        "and so was the audit row"
    );

    // failure *after* a successful write also rolls it back
    let e = s.apply(&[fx(json!({"op": "update", "collection": "products", "id": "sku-1", "inc": {"stock": 100}})),
                      fx(json!({"op": "update", "collection": "products", "id": "ghost", "inc": {"stock": 1}}))]).await.unwrap_err();
    assert!(matches!(e, ApplyError::NotFound { index: 1, .. }));
    assert_eq!(
        s.get("products", "sku-1").await.unwrap().unwrap()["stock"],
        3
    );

    // ---- insert conflicts, ifMissing, upsert
    assert!(matches!(
        s.apply(&[Effect::insert("orders", "o1", json!({}))]).await,
        Err(ApplyError::Conflict { .. })
    ));
    let skipped = s
        .apply(&[
            fx(json!({"op": "delete", "collection": "orders", "id": "none", "ifMissing": "skip"})),
            Effect::insert("orders", "after-skip", json!({"id": "after-skip"})),
        ])
        .await
        .unwrap();
    assert_eq!(skipped[0].status, ResultStatus::Skipped);
    assert!(
        s.get("orders", "after-skip").await.unwrap().is_some(),
        "a skipped effect does not stop the plan"
    );
    let up = |n: i64| {
        fx(json!({"op": "upsert", "collection": "orders", "id": "u1", "doc": {"id": "u1", "n": n}}))
    };
    assert_eq!(
        s.apply(&[up(1)]).await.unwrap()[0].status,
        ResultStatus::Inserted
    );
    let r = s.apply(&[up(2)]).await.unwrap();
    assert_eq!(
        (r[0].status, r[0].version),
        (ResultStatus::Replaced, Some(2))
    );

    // ---- operators through the store
    s.insert(
        "ops",
        "1",
        &json!({"id": "1", "stock": 10, "tags": ["a"], "meta": {"x": 1}, "tmp": 1}),
    )
    .await
    .unwrap();
    let r = s.apply(&[fx(json!({"op": "update", "collection": "ops", "id": "1", "set": {"meta.y": 2}, "inc": {"stock": -3, "hits": 1},
                                "push": {"tags": "b"}, "unset": ["tmp"], "merge": {"meta": {"x": null}}}))]).await.unwrap();
    assert_eq!(
        r[0].doc.clone().unwrap(),
        json!({"id": "1", "stock": 7, "hits": 1, "tags": ["a", "b"], "meta": {"y": 2}})
    );

    // ---- operators that cannot apply fail the whole plan with a precise message
    let e = s
        .apply(&[fx(
            json!({"op": "update", "collection": "ops", "id": "1", "inc": {"tags": 1}}),
        )])
        .await
        .unwrap_err();
    assert!(
        matches!(&e, ApplyError::Invalid { message, .. } if message.contains("not a number")),
        "{e}"
    );
    let e = s
        .apply(&[
            fx(json!({"op": "update", "collection": "ops", "id": "1", "set": {"a": 1}})),
            fx(json!({"op": "update", "collection": "ops", "id": "1", "push": {"stock": 1}})),
        ])
        .await
        .unwrap_err();
    assert!(matches!(e, ApplyError::Invalid { index: 1, .. }));
    assert!(
        s.get("ops", "1").await.unwrap().unwrap().get("a").is_none(),
        "the earlier set was rolled back"
    );
    let e = s
        .apply(&[fx(json!({"op": "update", "collection": "ops", "id": "1"}))])
        .await
        .unwrap_err();
    assert!(
        matches!(e, ApplyError::Invalid { .. }),
        "malformed effects are rejected before touching the database"
    );

    // ====================================================================================
    // concurrency
    // ====================================================================================

    // no overselling: 30 in stock, 60 buyers racing
    s.insert("stock", "item", &json!({"id": "item", "stock": 30}))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..60 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            s.apply(&[fx(json!({"op": "update", "collection": "stock", "id": "item", "inc": {"stock": -1}, "require": {"stock": {"gte": 1}}}))]).await
        }));
    }
    let (mut ok, mut refused) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(ApplyError::Precondition { .. }) => refused += 1,
            Err(e) => panic!("unexpected error under contention: {e}"),
        }
    }
    assert_eq!(
        (ok, refused),
        (30, 30),
        "exactly the available stock was sold"
    );
    assert_eq!(s.get("stock", "item").await.unwrap().unwrap()["stock"], 0);

    // no lost updates: 50 unconditional increments
    s.insert("counters", "c", &json!({"id": "c", "n": 0}))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            s.apply(&[fx(
                json!({"op": "update", "collection": "counters", "id": "c", "inc": {"n": 1}}),
            )])
            .await
            .unwrap()
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let (doc, version) = s.get_versioned("counters", "c").await.unwrap().unwrap();
    assert_eq!(
        (doc["n"].clone(), version),
        (json!(50), 51),
        "every increment counted, every write versioned"
    );

    // duplicate ids: exactly one insert wins
    let mut tasks = Vec::new();
    for i in 0..20 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            s.apply(&[Effect::insert(
                "dups",
                "same",
                json!({"id": "same", "who": i}),
            )])
            .await
        }));
    }
    let (mut won, mut conflicts) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => won += 1,
            Err(ApplyError::Conflict { .. }) => conflicts += 1,
            Err(e) => panic!("unexpected: {e}"),
        }
    }
    assert_eq!((won, conflicts), (1, 19));

    // optimistic concurrency: of 20 writers holding version 1, one wins
    s.insert("occ", "d", &json!({"id": "d", "v": 0}))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..20 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            s.apply(&[fx(json!({"op": "update", "collection": "occ", "id": "d", "set": {"v": i + 1}, "expectVersion": 1}))]).await
        }));
    }
    let (mut won, mut mismatched) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => won += 1,
            Err(ApplyError::VersionMismatch { .. }) => mismatched += 1,
            Err(e) => panic!("unexpected: {e}"),
        }
    }
    assert_eq!(
        (won, mismatched),
        (1, 19),
        "If-Match gives exactly one winner"
    );

    // transfers in both directions touch the same two rows in opposite orders: money is conserved,
    // and (on PostgreSQL) deadlocks are retried rather than surfaced
    for id in ["a", "b"] {
        s.insert("accounts", id, &json!({"id": id, "balance": 100}))
            .await
            .unwrap();
    }
    let mut tasks = Vec::new();
    for i in 0..40 {
        let s = s.clone();
        let (from, to) = if i % 2 == 0 { ("a", "b") } else { ("b", "a") };
        tasks.push(tokio::spawn(async move {
            s.apply(&[
                fx(json!({"op": "update", "collection": "accounts", "id": from, "inc": {"balance": -1}, "require": {"balance": {"gte": 1}}})),
                fx(json!({"op": "update", "collection": "accounts", "id": to, "inc": {"balance": 1}})),
            ])
            .await
        }));
    }
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) | Err(ApplyError::Precondition { .. }) => {}
            Err(e) => panic!("transfer failed (deadlock not handled?): {e}"),
        }
    }
    let total: i64 = s
        .list(
            "accounts",
            &ListQuery {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .iter()
        .map(|d| d["balance"].as_i64().unwrap())
        .sum();
    assert_eq!(total, 200, "no money created or destroyed");

    // read-only wrapper refuses plans
    let ro = stepwise_store::ReadOnly(s.clone());
    let e = ro
        .apply(&[Effect::insert("ro", "1", json!({}))])
        .await
        .unwrap_err();
    assert!(e.to_string().starts_with("this run is read-only"));
    assert!(
        ro.get("products", "sku-1").await.unwrap().is_some(),
        "reads still work"
    );
    let _ = OpKind::Insert;
}
