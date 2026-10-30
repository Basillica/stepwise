//! No-code data rules through the CRUD executor, on real backends.
use std::sync::Arc;

use serde_json::{json, Value};
use stepwise_store::{
    crud::{run, CrudError, CrudInput, CrudOutput, CrudPlan, IdKind, Op, RuleInput},
    rules::DataRules,
    DocStore, ListQuery, PostgresStore, SqliteStore,
};

fn plan(op: Op) -> CrudPlan {
    CrudPlan {
        op,
        collection: "orders".into(),
        id_param: Some("id".into()),
        id_field: "id".into(),
        id_kind: IdKind::String,
    }
}

fn rules(extra: Value) -> DataRules {
    let mut base = json!({
        "owner": "ownerId",
        "timestamps": {"created": "createdAt", "updated": "updatedAt", "deleted": "deletedAt"},
        "defaults": {"status": "open"},
        "readonly": ["status", "total"],
        "hide": ["internalNotes"],
        "soft_delete": "deleted",
        "audit": "audit"
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let r: DataRules = serde_json::from_value(base).unwrap();
    r.validate().unwrap();
    r
}

struct Call<'a> {
    s: &'a Arc<dyn DocStore>,
    rules: &'a DataRules,
    subject: Option<&'a str>,
}

impl Call<'_> {
    async fn go(
        &self,
        op: Op,
        id: Option<&str>,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> Result<CrudOutput, CrudError> {
        let vars = json!({"body": body.clone().unwrap_or(Value::Null), "query": {}, "path": {"id": id}, "identity": {"subject": self.subject, "roles": []}, "request_id": "req-1", "route": "R"});
        let input = CrudInput {
            path_id: id,
            body: body.as_ref(),
            query: &[],
            declared_query: &[],
            if_match,
            path: "/orders",
            rules: Some(RuleInput {
                rules: self.rules,
                subject: self.subject,
                vars: &vars,
            }),
        };
        run(self.s.as_ref(), &plan(op), &input, vec![]).await
    }
}

fn body(o: &CrudOutput) -> Value {
    o.body.clone().expect("a response body")
}

async fn all(s: &Arc<dyn DocStore>, coll: &str) -> Vec<Value> {
    s.list(
        coll,
        &ListQuery {
            limit: 100,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

async fn rules_contract(s: Arc<dyn DocStore>) {
    s.insert("products", "sku-1", &json!({"id": "sku-1", "stock": 5}))
        .await
        .unwrap();
    let with_effects = rules(
        json!({"effects": [{"op": "update", "collection": "products", "id": "${body.sku}", "inc": {"stock": "${-body.qty}"},
        "require": {"stock": {"gte": "${body.qty}"}}, "onFail": {"status": 409, "code": "out_of_stock", "message": "not enough stock"}}]}),
    );
    let plain = rules(json!({}));
    let (ann_create, ann, bob) = (
        Call {
            s: &s,
            rules: &with_effects,
            subject: Some("ann"),
        },
        Call {
            s: &s,
            rules: &plain,
            subject: Some("ann"),
        },
        Call {
            s: &s,
            rules: &plain,
            subject: Some("bob"),
        },
    );

    // ---- create: forged protected fields are ignored; defaults, owner, stamps, flag, effects, audit
    let forged = json!({"name": "A", "sku": "sku-1", "qty": 2, "status": "paid", "total": 999, "ownerId": "mallory", "createdAt": "FORGED",
                        "deleted": true, "internalNotes": "secret"});
    let out = ann_create
        .go(Op::Create, None, Some(forged), None)
        .await
        .unwrap();
    let created = body(&out);
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(
        (
            created["ownerId"].clone(),
            created["status"].clone(),
            created["deleted"].clone()
        ),
        (json!("ann"), json!("open"), json!(false))
    );
    assert!(
        created.get("total").is_none(),
        "a read-only field the client sent is dropped"
    );
    assert!(
        created["createdAt"].as_str().unwrap().ends_with('Z') && created["createdAt"] != "FORGED"
    );
    assert_eq!(created["updatedAt"], created["createdAt"]);
    assert!(
        created.get("internalNotes").is_none(),
        "hidden fields are removed from the response..."
    );
    assert_eq!(
        s.get("orders", &id).await.unwrap().unwrap()["internalNotes"],
        "secret",
        "...but not from storage"
    );
    assert_eq!(
        s.get("products", "sku-1").await.unwrap().unwrap()["stock"],
        3,
        "the templated effect ran: ${{-body.qty}} became -2"
    );
    let audit = all(&s, "audit").await;
    assert_eq!(
        (
            audit.len(),
            audit[0]["event"].clone(),
            audit[0]["by"].clone(),
            audit[0]["docId"].clone()
        ),
        (1, json!("create R"), json!("ann"), json!(id)),
        "{audit:?}"
    );

    // ---- a failing effect undoes everything, audit row included
    let e = ann_create
        .go(
            Op::Create,
            None,
            Some(json!({"name": "B", "sku": "sku-1", "qty": 9})),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.status, e.code.as_str(), e.message.as_str()),
        (409, "out_of_stock", "not enough stock")
    );
    assert_eq!(
        (
            all(&s, "orders").await.len(),
            all(&s, "audit").await.len(),
            s.get("products", "sku-1").await.unwrap().unwrap()["stock"].clone()
        ),
        (1, 1, json!(3))
    );
    // a template the request cannot satisfy is the client's error, with the field named
    let e = ann_create
        .go(Op::Create, None, Some(json!({"name": "C", "qty": 1})), None)
        .await
        .unwrap_err();
    assert_eq!((e.status, e.code.as_str()), (400, "invalid_request"));
    assert!(e.message.contains("body.sku"), "{}", e.message);

    // ---- owner scoping: other users cannot see or touch it, and cannot tell it exists
    let missing = |e: CrudError| (e.status, e.code);
    assert_eq!(
        missing(bob.go(Op::Read, Some(&id), None, None).await.unwrap_err()),
        (404, "not_found".to_string())
    );
    assert_eq!(
        missing(
            bob.go(
                Op::Replace,
                Some(&id),
                Some(json!({"name": "stolen"})),
                None
            )
            .await
            .unwrap_err()
        ),
        (404, "not_found".to_string())
    );
    assert_eq!(
        missing(
            bob.go(Op::Merge, Some(&id), Some(json!({"name": "stolen"})), None)
                .await
                .unwrap_err()
        ),
        (404, "not_found".to_string())
    );
    assert_eq!(
        missing(bob.go(Op::Delete, Some(&id), None, None).await.unwrap_err()),
        (404, "not_found".to_string())
    );
    assert_eq!(
        s.get("orders", &id).await.unwrap().unwrap()["name"],
        "A",
        "untouched"
    );
    let l = bob.go(Op::List, None, None, None).await.unwrap();
    assert_eq!(
        (
            body(&l),
            l.headers
                .iter()
                .find(|(k, _)| k == "x-total-count")
                .map(|(_, v)| v.as_str())
        ),
        (json!([]), Some("0"))
    );
    let l = ann.go(Op::List, None, None, None).await.unwrap();
    assert_eq!(body(&l).as_array().unwrap().len(), 1);
    assert!(
        body(&l)[0].get("internalNotes").is_none(),
        "lists hide fields too"
    );
    let anon = Call {
        s: &s,
        rules: &plain,
        subject: None,
    };
    assert_eq!(
        anon.go(Op::List, None, None, None)
            .await
            .unwrap_err()
            .status,
        401,
        "an owner-scoped route needs a signed-in user"
    );
    let r = ann.go(Op::Read, Some(&id), None, None).await.unwrap();
    assert!(body(&r).get("internalNotes").is_none());

    // ---- replace: read-only fields, owner and createdAt survive; the client cannot rewrite them
    let rep = ann.go(Op::Replace, Some(&id), Some(json!({"name": "A2", "status": "paid", "ownerId": "mallory", "createdAt": "X", "deleted": true, "internalNotes": "s2"})), None).await.unwrap();
    let rep = body(&rep);
    assert_eq!(
        (
            rep["name"].clone(),
            rep["status"].clone(),
            rep["ownerId"].clone(),
            rep["deleted"].clone()
        ),
        (json!("A2"), json!("open"), json!("ann"), json!(false))
    );
    assert_eq!(
        rep["createdAt"], created["createdAt"],
        "createdAt is carried over from the stored document"
    );
    assert!(rep.get("total").is_none() && rep["updatedAt"].is_string());
    // merge: protected fields are stripped from the patch
    let m = body(
        &ann.go(
            Op::Merge,
            Some(&id),
            Some(json!({"name": "A3", "status": "paid", "ownerId": "x", "deleted": true})),
            None,
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        (
            m["name"].clone(),
            m["status"].clone(),
            m["ownerId"].clone(),
            m["deleted"].clone()
        ),
        (json!("A3"), json!("open"), json!("ann"), json!(false))
    );
    // optimistic concurrency still works under rules
    let stale = ann
        .go(
            Op::Replace,
            Some(&id),
            Some(json!({"name": "late"})),
            Some("\"1\""),
        )
        .await
        .unwrap_err();
    assert_eq!(stale.status, 412);

    // ---- soft delete: gone from every read, kept in storage, audited
    let del = ann.go(Op::Delete, Some(&id), None, None).await.unwrap();
    assert!(
        del.body.is_none() && del.headers.is_empty(),
        "to the client it is an ordinary DELETE"
    );
    let stored = s.get("orders", &id).await.unwrap().unwrap();
    assert_eq!(
        (stored["deleted"].clone(), stored["deletedAt"].is_string()),
        (json!(true), true),
        "marked, stamped and still stored"
    );
    assert_eq!(
        ann.go(Op::Read, Some(&id), None, None)
            .await
            .unwrap_err()
            .status,
        404
    );
    let l = ann.go(Op::List, None, None, None).await.unwrap();
    assert_eq!(
        (
            body(&l),
            l.headers
                .iter()
                .find(|(k, _)| k == "x-total-count")
                .map(|(_, v)| v.as_str())
        ),
        (json!([]), Some("0"))
    );
    for op in [Op::Delete, Op::Replace, Op::Merge] {
        let b = if op == Op::Delete {
            None
        } else {
            Some(json!({"name": "zombie"}))
        };
        assert_eq!(
            ann.go(op, Some(&id), b, None).await.unwrap_err().status,
            404,
            "{op:?} on a soft-deleted document"
        );
    }
    let events: Vec<String> = all(&s, "audit")
        .await
        .iter()
        .map(|a| a["event"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        events,
        ["create R", "replace R", "merge R", "delete R"],
        "one ledger row per successful write, none for refused ones"
    );

    // ---- legacy rows (no flag yet) still behave: visible, and soft-deletable
    s.insert(
        "orders",
        "legacy",
        &json!({"id": "legacy", "ownerId": "ann", "name": "old"}),
    )
    .await
    .unwrap();
    let l = ann.go(Op::List, None, None, None).await.unwrap();
    assert_eq!(
        body(&l).as_array().unwrap().len(),
        1,
        "a row without the flag counts as not deleted"
    );
    ann.go(Op::Delete, Some("legacy"), None, None)
        .await
        .unwrap();
    assert_eq!(
        ann.go(Op::Read, Some("legacy"), None, None)
            .await
            .unwrap_err()
            .status,
        404
    );

    // ---- rules without owner or soft delete: just defaults/hide/stamps
    let light = rules(
        json!({"owner": null, "soft_delete": null, "timestamps": {"created": "createdAt"}, "audit": null}),
    );
    let light = DataRules {
        owner: None,
        soft_delete: None,
        audit: None,
        ..light
    };
    light.validate().unwrap();
    let open = Call {
        s: &s,
        rules: &light,
        subject: None,
    };
    let c = body(
        &open
            .go(
                Op::Create,
                None,
                Some(json!({"name": "pub", "status": "x", "internalNotes": "n"})),
                None,
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        (
            c["status"].clone(),
            c.get("ownerId").is_none(),
            c["createdAt"].is_string(),
            c.get("internalNotes").is_none()
        ),
        (json!("open"), true, true, true)
    );
    assert!(open
        .go(Op::Delete, Some(c["id"].as_str().unwrap()), None, None)
        .await
        .is_ok());
    assert!(
        s.get("orders", c["id"].as_str().unwrap())
            .await
            .unwrap()
            .is_none(),
        "without soft_delete a DELETE really deletes"
    );
}

#[tokio::test]
async fn sqlite_data_rules() {
    let dir = tempfile::tempdir().unwrap();
    rules_contract(SqliteStore::open(dir.path().join("r.db").to_str().unwrap(), 4, 5000).unwrap())
        .await;
}

#[tokio::test]
async fn postgres_data_rules() {
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
    rules_contract(store.clone()).await;
    store.drop_schema_cascade().await.unwrap();
}
