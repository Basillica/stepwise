//! The CRUD executor against real backends: ETag / If-Match, and effects queued by scripts joining
//! the route's own transaction. SQLite always; PostgreSQL when STEPWISE_TEST_POSTGRES_URL is set.
use std::sync::Arc;

use serde_json::{json, Value};
use stepwise_store::{
    crud::{run, CrudError, CrudInput, CrudOutput, CrudPlan, IdKind, Op},
    DocStore, Effect, PostgresStore, SqliteStore,
};

fn plan(op: Op, coll: &str, kind: IdKind) -> CrudPlan {
    CrudPlan {
        op,
        collection: coll.into(),
        id_param: Some("id".into()),
        id_field: "id".into(),
        id_kind: kind,
    }
}

fn fx(v: Value) -> Effect {
    serde_json::from_value(v).unwrap()
}

#[derive(Default)]
struct In {
    id: Option<&'static str>,
    body: Option<Value>,
    query: Vec<(String, String)>,
    declared: Vec<&'static str>,
    if_match: Option<&'static str>,
}

async fn call(
    s: &Arc<dyn DocStore>,
    p: &CrudPlan,
    i: In,
    pending: Vec<Effect>,
) -> Result<CrudOutput, CrudError> {
    let input = CrudInput {
        path_id: i.id,
        body: i.body.as_ref(),
        query: &i.query,
        declared_query: &i.declared,
        if_match: i.if_match,
        path: "/pets",
        rules: None,
    };
    run(s.as_ref(), p, &input, pending).await
}

fn body_of(o: &CrudOutput) -> Value {
    o.body.clone().expect("a response body")
}

fn header<'a>(o: &'a CrudOutput, k: &str) -> Option<&'a str> {
    o.headers
        .iter()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.as_str())
}

async fn crud_contract(s: Arc<dyn DocStore>) {
    let create = plan(Op::Create, "pets", IdKind::Integer);
    let read = plan(Op::Read, "pets", IdKind::Integer);
    let replace = plan(Op::Replace, "pets", IdKind::Integer);
    let merge = plan(Op::Merge, "pets", IdKind::Integer);
    let delete = plan(Op::Delete, "pets", IdKind::Integer);
    let list = plan(Op::List, "pets", IdKind::Integer);
    let body = |v: Value| In {
        body: Some(v),
        ..Default::default()
    };

    // ---- create: sequence ids, Location, ETag; client ids honoured and the sequence moves past them
    let r = call(
        &s,
        &create,
        body(json!({"name": "Rex", "tag": "dog"})),
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(body_of(&r), json!({"id": 1, "name": "Rex", "tag": "dog"}));
    assert_eq!(
        (header(&r, "etag"), header(&r, "location")),
        (Some("\"1\""), Some("/pets/1"))
    );
    assert_eq!(
        call(&s, &create, body(json!({"id": 10, "name": "Ada"})), vec![])
            .await
            .unwrap()
            .body
            .unwrap()["id"],
        10
    );
    assert_eq!(
        call(&s, &create, body(json!({"name": "Bo"})), vec![])
            .await
            .unwrap()
            .body
            .unwrap()["id"],
        11
    );
    let e = call(&s, &create, body(json!({"id": 10, "name": "Dup"})), vec![])
        .await
        .unwrap_err();
    assert_eq!((e.status, e.code.as_str()), (409, "conflict"));
    assert_eq!(
        call(&s, &create, body(json!({"id": "x"})), vec![])
            .await
            .unwrap_err()
            .code,
        "invalid_id"
    );
    assert_eq!(
        call(&s, &create, In::default(), vec![])
            .await
            .unwrap_err()
            .code,
        "invalid_body"
    );

    // ---- read: ETag is the version; 404
    let r = call(
        &s,
        &read,
        In {
            id: Some("1"),
            ..Default::default()
        },
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(
        (body_of(&r)["name"].clone(), header(&r, "etag")),
        (json!("Rex"), Some("\"1\""))
    );
    assert_eq!(
        call(
            &s,
            &read,
            In {
                id: Some("404"),
                ..Default::default()
            },
            vec![]
        )
        .await
        .unwrap_err()
        .status,
        404
    );

    // ---- optimistic concurrency with If-Match
    let r = call(
        &s,
        &replace,
        In {
            id: Some("1"),
            body: Some(json!({"name": "Max"})),
            ..Default::default()
        },
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(
        (header(&r, "etag"), body_of(&r)),
        (Some("\"2\""), json!({"id": 1, "name": "Max"})),
        "the URL's id wins; version bumped"
    );
    let stale = call(
        &s,
        &replace,
        In {
            id: Some("1"),
            body: Some(json!({"name": "Stale"})),
            if_match: Some("\"1\""),
            ..Default::default()
        },
        vec![],
    )
    .await
    .unwrap_err();
    assert_eq!(
        (stale.status, stale.code.as_str()),
        (412, "precondition_failed")
    );
    assert_eq!(
        stale.headers[0].1, "\"2\"",
        "the 412 tells the client the current version"
    );
    assert_eq!(
        s.get("pets", "1").await.unwrap().unwrap()["name"],
        "Max",
        "a refused write changed nothing"
    );
    let r = call(
        &s,
        &merge,
        In {
            id: Some("1"),
            body: Some(json!({"tag": "cat", "id": 99})),
            if_match: Some("\"2\""),
            ..Default::default()
        },
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(
        (header(&r, "etag"), body_of(&r)),
        (Some("\"3\""), json!({"id": 1, "name": "Max", "tag": "cat"})),
        "ids are immutable"
    );
    assert_eq!(
        call(
            &s,
            &merge,
            In {
                id: Some("1"),
                body: Some(json!({"a": 1})),
                if_match: Some("garbage"),
                ..Default::default()
            },
            vec![]
        )
        .await
        .unwrap_err()
        .status,
        400
    );
    assert_eq!(
        call(
            &s,
            &merge,
            In {
                id: Some("404"),
                body: Some(json!({"a": 1})),
                ..Default::default()
            },
            vec![]
        )
        .await
        .unwrap_err()
        .status,
        404
    );
    assert!(
        call(
            &s,
            &replace,
            In {
                id: Some("1"),
                body: Some(json!({"name": "Any"})),
                if_match: Some("*"),
                ..Default::default()
            },
            vec![]
        )
        .await
        .is_ok(),
        "* matches any version"
    );
    let stale_del = call(
        &s,
        &delete,
        In {
            id: Some("1"),
            if_match: Some("\"1\""),
            ..Default::default()
        },
        vec![],
    )
    .await
    .unwrap_err();
    assert_eq!(stale_del.status, 412);
    assert!(call(
        &s,
        &delete,
        In {
            id: Some("1"),
            if_match: Some("\"4\""),
            ..Default::default()
        },
        vec![]
    )
    .await
    .unwrap()
    .body
    .is_none());
    assert_eq!(
        call(
            &s,
            &delete,
            In {
                id: Some("1"),
                ..Default::default()
            },
            vec![]
        )
        .await
        .unwrap_err()
        .status,
        404
    );

    // ---- list: only declared params filter; sort; paging; total
    for (n, t) in [("b", "dog"), ("a", "cat"), ("c", "dog")] {
        call(&s, &create, body(json!({"name": n, "tag": t})), vec![])
            .await
            .unwrap();
    }
    let q = |pairs: &[(&str, &str)]| In {
        query: pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        declared: vec!["tag"],
        ..Default::default()
    };
    let r = call(&s, &list, q(&[("tag", "dog")]), vec![]).await.unwrap();
    assert_eq!(
        (
            body_of(&r).as_array().unwrap().len(),
            header(&r, "x-total-count")
        ),
        (2, Some("2"))
    );
    let r = call(&s, &list, q(&[("name", "a")]), vec![]).await.unwrap();
    assert_eq!(
        body_of(&r).as_array().unwrap().len(),
        5,
        "`name` is not a declared parameter, so it does not filter"
    );
    let r = call(
        &s,
        &list,
        q(&[("sort", "-name"), ("limit", "2"), ("offset", "1")]),
        vec![],
    )
    .await
    .unwrap();
    let names: Vec<String> = body_of(&r)
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        (names, header(&r, "x-total-count")),
        (vec!["b".to_string(), "a".to_string()], Some("5"))
    );
    assert_eq!(
        call(&s, &list, q(&[("limit", "0")]), vec![])
            .await
            .unwrap_err()
            .code,
        "invalid_query"
    );

    // ====================================================================================
    // effects queued by scripts share the route's transaction
    // ====================================================================================
    s.insert("products", "sku-1", &json!({"id": "sku-1", "stock": 5}))
        .await
        .unwrap();
    let order_plan = plan(Op::Create, "orders", IdKind::String);
    let decrement = |qty: i64| {
        fx(
            json!({"op": "update", "collection": "products", "id": "sku-1", "inc": {"stock": -qty},
                  "require": {"stock": {"gte": qty}}, "onFail": {"status": 409, "code": "out_of_stock", "message": "not enough stock"}}),
        )
    };
    let audit = || fx(json!({"op": "insert", "collection": "audit", "doc": {"event": "order"}}));

    // success: the order, the stock change and the audit row all happen
    let r = call(
        &s,
        &order_plan,
        body(json!({"id": "o1", "sku": "sku-1", "qty": 2})),
        vec![decrement(2), audit()],
    )
    .await
    .unwrap();
    assert_eq!(body_of(&r)["id"], "o1");
    assert_eq!(
        s.get("products", "sku-1").await.unwrap().unwrap()["stock"],
        3
    );
    assert_eq!(s.count("audit", &[]).await.unwrap(), 1);

    // a failing script effect stops the route's own write
    let e = call(
        &s,
        &order_plan,
        body(json!({"id": "o2", "sku": "sku-1", "qty": 9})),
        vec![audit(), decrement(9)],
    )
    .await
    .unwrap_err();
    assert_eq!(
        (e.status, e.code.as_str(), e.message.as_str()),
        (409, "out_of_stock", "not enough stock"),
        "the script's own error reaches the client"
    );
    assert!(
        s.get("orders", "o2").await.unwrap().is_none(),
        "the order was not created"
    );
    assert_eq!(
        s.count("audit", &[]).await.unwrap(),
        1,
        "the audit row queued before the failure was rolled back too"
    );

    // and the other way round: the *route's own write* failing undoes the script's effects
    let e = call(
        &s,
        &order_plan,
        body(json!({"id": "o1", "sku": "sku-1", "qty": 1})),
        vec![decrement(1), audit()],
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "conflict", "o1 already exists");
    assert_eq!(
        s.get("products", "sku-1").await.unwrap().unwrap()["stock"],
        3,
        "the stock decrement was rolled back with it"
    );
    assert_eq!(s.count("audit", &[]).await.unwrap(), 1);

    // an If-Match failure on the route's write undoes the script's effects as well
    s.insert("notes", "n1", &json!({"id": "n1", "text": "v1"}))
        .await
        .unwrap();
    s.replace("notes", "n1", &json!({"id": "n1", "text": "v2"}))
        .await
        .unwrap();
    let rep = CrudPlan {
        id_param: Some("id".into()),
        ..plan(Op::Replace, "notes", IdKind::String)
    };
    let e = call(
        &s,
        &rep,
        In {
            id: Some("n1"),
            body: Some(json!({"text": "mine"})),
            if_match: Some("\"1\""),
            ..Default::default()
        },
        vec![audit()],
    )
    .await
    .unwrap_err();
    assert_eq!(e.status, 412);
    assert_eq!(
        s.count("audit", &[]).await.unwrap(),
        1,
        "queued audit row not written"
    );

    // reads honour queued effects (committed first), lists too
    let rd = plan(Op::Read, "products", IdKind::String);
    let r = call(
        &s,
        &rd,
        In {
            id: Some("sku-1"),
            ..Default::default()
        },
        vec![audit()],
    )
    .await
    .unwrap();
    assert_eq!(body_of(&r)["stock"], 3);
    assert_eq!(s.count("audit", &[]).await.unwrap(), 2);
    // a failing effect on a read fails the read
    let e = call(
        &s,
        &rd,
        In {
            id: Some("sku-1"),
            ..Default::default()
        },
        vec![decrement(99)],
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "out_of_stock");

    // opted-out routes (Op::None) with queued effects still commit them
    let none = plan(Op::None, "x", IdKind::String);
    call(&s, &none, In::default(), vec![audit()]).await.unwrap();
    assert_eq!(s.count("audit", &[]).await.unwrap(), 3);

    // concurrency through the HTTP-shaped path: only available stock is sold
    s.insert("products", "sku-2", &json!({"id": "sku-2", "stock": 10}))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..25 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let p = plan(Op::Create, "orders", IdKind::String);
            let dec = fx(json!({"op": "update", "collection": "products", "id": "sku-2", "inc": {"stock": -1},
                                "require": {"stock": {"gte": 1}}, "onFail": {"status": 409, "code": "out_of_stock"}}));
            let b = json!({"id": format!("c{i}"), "sku": "sku-2", "qty": 1});
            let input = CrudInput { path_id: None, body: Some(&b), query: &[], declared_query: &[], if_match: None, path: "/orders", rules: None };
            run(s.as_ref(), &p, &input, vec![dec]).await
        }));
    }
    let (mut sold, mut refused) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => sold += 1,
            Err(e) if e.code == "out_of_stock" => refused += 1,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert_eq!((sold, refused), (10, 15));
    assert_eq!(
        s.get("products", "sku-2").await.unwrap().unwrap()["stock"],
        0
    );
    let created = s
        .count("orders", &[("sku".into(), "sku-2".into())])
        .await
        .unwrap();
    assert_eq!(created, 10, "exactly as many orders exist as units sold: no order without stock, no stock without an order");
}

#[tokio::test]
async fn sqlite_crud_executor() {
    let dir = tempfile::tempdir().unwrap();
    crud_contract(SqliteStore::open(dir.path().join("c.db").to_str().unwrap(), 4, 5000).unwrap())
        .await;
}

#[tokio::test]
async fn postgres_crud_executor() {
    let Ok(url) = std::env::var("STEPWISE_TEST_POSTGRES_URL") else {
        eprintln!("SKIP: set STEPWISE_TEST_POSTGRES_URL");
        return;
    };
    let store = PostgresStore::new(
        &url,
        &format!("t_{}", uuid::Uuid::new_v4().simple()),
        6,
        None,
    )
    .unwrap();
    crud_contract(store.clone()).await;
    store.drop_schema_cascade().await.unwrap();
}
