//! The read engine on real backends. SQLite always; PostgreSQL when STEPWISE_TEST_POSTGRES_URL is set.
use std::sync::{
    atomic::{AtomicUsize, Ordering::SeqCst},
    Arc,
};

use async_trait::async_trait;
use serde_json::{json, Value};
use stepwise_store::{
    effects::{ApplyError, Effect, OpResult},
    query::{parse_query, Engine, Limits, QueryError, Relations},
    DocStore, ListQuery, PostgresStore, SqliteStore,
};

/// Counts calls so the test can prove relations are batched (no N+1).
struct Counting {
    inner: Arc<dyn DocStore>,
    list_rows: AtomicUsize,
    get_many: AtomicUsize,
}

#[async_trait]
impl DocStore for Counting {
    async fn get_versioned(&self, c: &str, id: &str) -> anyhow::Result<Option<(Value, u64)>> {
        self.inner.get_versioned(c, id).await
    }
    async fn list_rows(&self, c: &str, q: &ListQuery) -> anyhow::Result<Vec<(String, Value)>> {
        self.list_rows.fetch_add(1, SeqCst);
        self.inner.list_rows(c, q).await
    }
    async fn get_many(&self, c: &str, ids: &[String]) -> anyhow::Result<Vec<(String, Value)>> {
        self.get_many.fetch_add(1, SeqCst);
        self.inner.get_many(c, ids).await
    }
    async fn count_matching(&self, c: &str, q: &ListQuery) -> anyhow::Result<u64> {
        self.inner.count_matching(c, q).await
    }
    async fn next_sequence(&self, c: &str) -> anyhow::Result<i64> {
        self.inner.next_sequence(c).await
    }
    async fn bump_sequence(&self, c: &str, n: i64) -> anyhow::Result<()> {
        self.inner.bump_sequence(c, n).await
    }
    async fn collections(&self) -> anyhow::Result<Vec<(String, u64)>> {
        self.inner.collections().await
    }
    async fn clear(&self, c: &str) -> anyhow::Result<u64> {
        self.inner.clear(c).await
    }
    async fn ping(&self) -> anyhow::Result<()> {
        self.inner.ping().await
    }
    async fn apply(&self, plan: &[Effect]) -> Result<Vec<OpResult>, ApplyError> {
        self.inner.apply(plan).await
    }
}

fn rels() -> Relations {
    serde_json::from_value(json!({
        "orders": {
            "items":    { "from": "orderItems", "by": "orderId" },
            "customer": { "from": "customers",  "id": "customerId" }
        },
        "orderItems": { "product": { "from": "products", "id": "productId" } }
    }))
    .unwrap()
}

async fn seed(s: &Arc<dyn DocStore>) {
    for (id, doc) in [
        (
            "c1",
            json!({"id": "c1", "name": "Ann", "tier": "gold", "address": {"city": "Oslo", "zip": "0150"}}),
        ),
        (
            "c2",
            json!({"id": "c2", "name": "Bob", "tier": "silver", "address": {"city": "Rome"}}),
        ),
        ("c3", json!({"id": "c3", "name": "Cy", "tier": "gold"})),
    ] {
        s.insert("customers", id, &doc).await.unwrap();
    }
    for (id, name, price) in [
        ("p1", "Pen", json!(2)),
        ("p2", "Ink", json!(15)),
        ("p3", "Pad", json!(7.5)),
    ] {
        s.insert(
            "products",
            id,
            &json!({"id": id, "name": name, "price": price}),
        )
        .await
        .unwrap();
    }
    for i in 1..=12u32 {
        let customer = match i {
            1 | 5 | 9 => Some("c1"),
            2 | 6 | 10 => Some("c2"),
            3 | 7 | 11 => Some("c3"),
            12 => Some("c99"), // dangling reference
            _ => None,         // no reference at all
        };
        let status = ["void", "open", "paid"][(i % 3) as usize];
        let mut doc =
            json!({"id": format!("o{i}"), "status": status, "total": i * 10, "createdAt": i});
        if let Some(c) = customer {
            doc["customerId"] = json!(c);
        }
        s.insert("orders", &format!("o{i}"), &doc).await.unwrap();
        s.insert("orderItems", &format!("i{i}a"), &json!({"id": format!("i{i}a"), "orderId": format!("o{i}"), "productId": "p1", "qty": i % 3 + 1})).await.unwrap();
        s.insert("orderItems", &format!("i{i}b"), &json!({"id": format!("i{i}b"), "orderId": format!("o{i}"), "productId": "p2", "qty": i % 2 + 1})).await.unwrap();
    }
}

async fn run_with(
    s: &dyn DocStore,
    q: Value,
    limits: Limits,
    deny: &'static [&'static str],
) -> Result<Value, QueryError> {
    let query = parse_query(&q, &rels(), limits.max_depth).map_err(QueryError::Invalid)?;
    let can_read = move |c: &str| {
        if deny.contains(&c) {
            Err(format!("collection `{c}` is not readable here"))
        } else {
            Ok(())
        }
    };
    Engine {
        store: s,
        limits,
        can_read: &can_read,
    }
    .run(&query)
    .await
}

async fn run(s: &dyn DocStore, q: Value) -> Value {
    run_with(s, q, Limits::default(), &[])
        .await
        .unwrap_or_else(|e| panic!("query failed: {e}"))
}

fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .collect()
}

async fn query_contract(raw: Arc<dyn DocStore>) {
    seed(&raw).await;
    let s = raw.as_ref();

    // ---- direct path (database sorts, pages and counts)
    let r = run(s, json!({"from": "orders", "where": {"status": "open"}, "orderBy": ["-createdAt"], "limit": 3, "total": true})).await;
    assert_eq!(
        (ids(&r), r["total"].clone()),
        (vec!["o10".to_string(), "o7".into(), "o4".into()], json!(4))
    );
    let r = run(
        s,
        json!({"from": "orders", "orderBy": "createdAt", "limit": 2, "offset": 10}),
    )
    .await;
    assert_eq!(ids(&r), ["o11", "o12"], "offset");

    // ---- by id
    let r = run(
        s,
        json!({"from": "orders", "id": "o1", "select": ["id", "total"]}),
    )
    .await;
    assert_eq!(r, json!({"data": {"id": "o1", "total": 10}}));
    assert_eq!(
        run(s, json!({"from": "orders", "id": "nope"})).await,
        json!({"data": null})
    );
    assert_eq!(
        run(
            s,
            json!({"from": "orders", "id": "o1", "where": {"status": "paid"}})
        )
        .await,
        json!({"data": null}),
        "a where on an id lookup applies"
    );

    // ---- projection: dotted paths, missing fields omitted, `*`
    let r = run(
        s,
        json!({"from": "customers", "orderBy": "id", "select": ["name", "address.city"]}),
    )
    .await;
    assert_eq!(
        r["data"],
        json!([{"name": "Ann", "address": {"city": "Oslo"}}, {"name": "Bob", "address": {"city": "Rome"}}, {"name": "Cy"}])
    );
    let r = run(s, json!({"from": "customers", "id": "c1", "select": ["*"]})).await;
    assert_eq!(r["data"]["address"]["zip"], "0150");

    // ---- scan path: operators the database does not evaluate
    let r = run(s, json!({"from": "orders", "where": {"total": {"gte": 100}, "status": {"in": ["open", "paid"]}}, "orderBy": [{"total": "desc"}, "id"], "total": true})).await;
    assert_eq!(
        (ids(&r), r["total"].clone()),
        (vec!["o11".to_string(), "o10".into()], json!(2))
    );
    let r = run(
        s,
        json!({"from": "orders", "where": {"customerId": {"exists": false}}, "orderBy": "id"}),
    )
    .await;
    assert_eq!(
        ids(&r),
        ["o4", "o8"],
        "exists:false finds documents without the field"
    );
    let r = run(s, json!({"from": "orders", "where": {"status": {"ne": "void"}, "total": {"lt": 50}}, "orderBy": ["-status", "createdAt"]})).await;
    assert_eq!(
        ids(&r),
        ["o2", "o1", "o4"],
        "multi-key ordering: status desc (paid, open), then createdAt asc: {r}"
    );
    let r = run(s, json!({"from": "orders", "where": {"status": "paid", "total": {"gt": 20}}, "orderBy": "createdAt", "limit": 2, "offset": 1, "total": true})).await;
    assert_eq!(
        (ids(&r), r["total"].clone()),
        (vec!["o8".to_string(), "o11".into()], json!(3)),
        "total counts before paging"
    );
    let r = run(s, json!({"from": "products", "where": {"price": 7.5}})).await;
    assert_eq!(ids(&r), ["p3"], "floats are compared in the engine");

    // ---- relations: has-many, belongs-to, nested, dangling and missing references
    let q = json!({"from": "orders", "orderBy": "createdAt", "limit": 12, "select": ["id",
        {"customer": {"from": "customers", "id": "customerId", "select": ["name"]}},
        {"items": {"from": "orderItems", "by": "orderId", "select": ["id", "qty", {"product": {"from": "products", "id": "productId", "select": ["name"]}}]}}]});
    let r = run(s, q.clone()).await;
    let data = r["data"].as_array().unwrap();
    assert_eq!(data.len(), 12);
    assert_eq!(data[0]["customer"], json!({"name": "Ann"}));
    assert_eq!(data[3]["customer"], Value::Null, "o4 has no customerId");
    assert_eq!(
        data[11]["customer"],
        Value::Null,
        "o12 points at a customer that does not exist"
    );
    assert_eq!(
        data[0]["items"],
        json!([{"id": "i1a", "qty": 2, "product": {"name": "Pen"}}, {"id": "i1b", "qty": 2, "product": {"name": "Ink"}}])
    );
    assert!(data
        .iter()
        .all(|o| o["items"].as_array().unwrap().len() == 2));
    assert!(
        data[0].get("total").is_none(),
        "only the selected fields are returned"
    );

    // ---- named relations behave exactly like inline ones
    let named = run(s, json!({"from": "orders", "orderBy": "createdAt", "limit": 12, "select": ["id", {"customer": {"rel": "customer", "select": ["name"]}},
        {"items": {"rel": "items", "select": ["id", "qty", {"product": {"rel": "product", "select": ["name"]}}]}}]})).await;
    assert_eq!(
        named, r,
        "named relations (config) and inline relations return the same result"
    );
    let r2 = run(
        s,
        json!({"from": "orders", "id": "o1", "select": ["id", {"items": "items"}]}),
    )
    .await;
    assert_eq!(r2["data"]["items"].as_array().unwrap().len(), 2);

    // ---- per-parent ordering and limit, relation filters
    let r = run(s, json!({"from": "orders", "orderBy": "createdAt", "limit": 3, "select": ["id", {"items": {"from": "orderItems", "by": "orderId", "select": ["id", "qty"], "orderBy": ["-qty", "id"], "limit": 1}}]})).await;
    assert_eq!(
        r["data"][0]["items"],
        json!([{"id": "i1a", "qty": 2}]),
        "tie broken by id"
    );
    assert_eq!(
        r["data"][1]["items"],
        json!([{"id": "i2a", "qty": 3}]),
        "the highest qty per order, one per order"
    );
    let r = run(s, json!({"from": "orders", "orderBy": "createdAt", "limit": 12, "select": ["id", {"big": {"from": "orderItems", "by": "orderId", "where": {"qty": {"gte": 3}}, "select": ["id"]}}]})).await;
    let with_big: Vec<String> = r["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| !o["big"].as_array().unwrap().is_empty())
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        with_big,
        ["o2", "o5", "o8", "o11"],
        "a relation's where filters the children; parents are kept"
    );

    // ---- permissions are checked on every collection the query touches
    let denied = run_with(s, q.clone(), Limits::default(), &["customers"])
        .await
        .unwrap_err();
    assert!(
        matches!(&denied, QueryError::Denied(m) if m.contains("customers")),
        "{denied}"
    );
    let denied = run_with(s, json!({"from": "orders"}), Limits::default(), &["orders"])
        .await
        .unwrap_err();
    assert!(matches!(denied, QueryError::Denied(_)));
    let denied = run_with(s, q.clone(), Limits::default(), &["products"])
        .await
        .unwrap_err();
    assert!(
        matches!(denied, QueryError::Denied(_)),
        "nested relation targets are checked too"
    );

    // ---- limits fail loudly instead of getting slow
    let tight = Limits {
        max_scan: 5,
        ..Limits::default()
    };
    let e = run_with(
        s,
        json!({"from": "orders", "where": {"total": {"gte": 1}}}),
        tight,
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&e, QueryError::TooBroad(m) if m.contains("equality filter")),
        "{e}"
    );
    let tight = Limits {
        max_docs: 5,
        ..Limits::default()
    };
    assert!(matches!(
        run_with(s, json!({"from": "orders", "limit": 12}), tight, &[]).await,
        Err(QueryError::TooBroad(_))
    ));
    let tight = Limits {
        max_limit: 4,
        ..Limits::default()
    };
    assert_eq!(
        ids(
            &run_with(s, json!({"from": "orders", "limit": 1000}), tight, &[])
                .await
                .unwrap()
        )
        .len(),
        4,
        "limit is clamped to max_limit"
    );
    let shallow = Limits {
        max_depth: 1,
        ..Limits::default()
    };
    assert!(
        run_with(s, q.clone(), shallow, &[]).await.is_err(),
        "nesting beyond max_depth is refused"
    );
    assert_eq!(
        ids(&run(s, json!({"from": "orders"})).await).len(),
        12,
        "default limit is 100"
    );
    assert!(
        run(s, json!({"from": "nothing"})).await["data"]
            .as_array()
            .unwrap()
            .is_empty(),
        "an unknown collection is just empty"
    );
}

/// Relations cost one query per level, however many parents there are.
async fn batching_contract(raw: Arc<dyn DocStore>) {
    seed(&raw).await;
    let calls = |limit: u32| {
        let raw = raw.clone();
        async move {
            let c = Counting {
                inner: raw,
                list_rows: AtomicUsize::new(0),
                get_many: AtomicUsize::new(0),
            };
            let q = json!({"from": "orders", "orderBy": "createdAt", "limit": limit, "select": ["id",
                {"customer": {"from": "customers", "id": "customerId", "select": ["name"]}},
                {"items": {"from": "orderItems", "by": "orderId", "select": ["qty", {"product": {"from": "products", "id": "productId", "select": ["name"]}}]}}]});
            let out = run(&c, q).await;
            assert_eq!(out["data"].as_array().unwrap().len(), limit as usize);
            (c.list_rows.load(SeqCst), c.get_many.load(SeqCst))
        }
    };
    let (small, large) = (calls(2).await, calls(12).await);
    assert_eq!(
        small, large,
        "work does not grow with the number of parent rows"
    );
    assert_eq!(
        large,
        (2, 2),
        "root + items (list), customers + products (get_many): no N+1"
    );
}

#[tokio::test]
async fn sqlite_query_engine() {
    let dir = tempfile::tempdir().unwrap();
    query_contract(SqliteStore::open(dir.path().join("a.db").to_str().unwrap(), 4, 5000).unwrap())
        .await;
    batching_contract(
        SqliteStore::open(dir.path().join("b.db").to_str().unwrap(), 4, 5000).unwrap(),
    )
    .await;
}

#[tokio::test]
async fn postgres_query_engine() {
    let Ok(url) = std::env::var("STEPWISE_TEST_POSTGRES_URL") else {
        eprintln!("SKIP: set STEPWISE_TEST_POSTGRES_URL");
        return;
    };
    for contract in 0..2 {
        let store = PostgresStore::new(
            &url,
            &format!("t_{}", uuid::Uuid::new_v4().simple()),
            6,
            None,
        )
        .unwrap();
        if contract == 0 {
            query_contract(store.clone()).await;
        } else {
            batching_contract(store.clone()).await;
        }
        store.drop_schema_cascade().await.unwrap();
    }
}
