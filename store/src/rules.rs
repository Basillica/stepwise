//! No-code data rules for a route: the common patterns, declared instead of scripted.
//!
//! ```yaml
//! routes:
//!   "POST /orders":
//!     data:
//!       owner: ownerId                      # scope to the signed-in user (stamped on create; others get 404)
//!       timestamps: { created: createdAt, updated: updatedAt, deleted: deletedAt }
//!       defaults: { status: open }
//!       readonly: [status, total]           # clients cannot set them (create strips, replace keeps, merge strips)
//!       hide: [internalNotes]               # removed from every response
//!       soft_delete: deleted                # DELETE sets this flag instead of removing; reads/lists skip flagged rows
//!       effects:                            # extra writes in the route's transaction, with ${…} templates
//!         - op: update
//!           collection: products
//!           id: "${body.sku}"
//!           inc: { stock: "${-body.qty}" }
//!           require: { stock: { gte: "${body.qty}" } }
//!           onFail: { status: 409, code: out_of_stock, message: "not enough stock" }
//!       audit: auditLog                     # one ledger row per write
//! ```
//!
//! Nothing here is a new write mechanism: every rule compiles down to the effects and conditions
//! of `effects.rs` (so they run in the route's single transaction, under row locks).
//!
//! **Templates.** A string that is exactly `"${path}"` is replaced by that JSON value, keeping its
//! type (`"${body.qty}"` stays a number); `"${-body.qty}"` negates a number; inside longer text the
//! value is interpolated as text. Variables: `body.*`, `query.*`, `path.*`, `identity.subject`,
//! `identity.roles`, `id` (the document id), `route`, `request_id`, `now` (ISO-8601 UTC).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{
    effects::{get_path, Effect},
    valid_collection, valid_field,
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timestamps {
    pub created: Option<String>,
    pub updated: Option<String>,
    /// Stamped when a soft delete happens (needs `soft_delete`).
    pub deleted: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DataRules {
    /// Field holding the owner's id (= the signed-in subject).
    pub owner: Option<String>,
    pub timestamps: Option<Timestamps>,
    /// Values set on create when the client did not provide them.
    pub defaults: Map<String, Value>,
    pub readonly: Vec<String>,
    pub hide: Vec<String>,
    /// Name of a boolean flag field; DELETE sets it instead of removing the document.
    pub soft_delete: Option<String>,
    /// Effect templates added to the route's transaction (before the route's own write).
    pub effects: Vec<Value>,
    /// Collection receiving one ledger row per write.
    pub audit: Option<String>,
}

impl DataRules {
    pub fn is_empty(&self) -> bool {
        *self == DataRules::default()
    }

    /// Names of the rules in effect (for introspection).
    pub fn summary(&self) -> Vec<&'static str> {
        let mut v = vec![];
        for (on, name) in [
            (self.owner.is_some(), "owner"),
            (self.timestamps.is_some(), "timestamps"),
            (!self.defaults.is_empty(), "defaults"),
            (!self.readonly.is_empty(), "readonly"),
            (!self.hide.is_empty(), "hide"),
            (self.soft_delete.is_some(), "soft_delete"),
            (!self.effects.is_empty(), "effects"),
            (self.audit.is_some(), "audit"),
        ] {
            if on {
                v.push(name);
            }
        }
        v
    }

    pub fn validate(&self) -> Result<(), String> {
        let simple = |what: &str, f: &str| -> Result<(), String> {
            if valid_field(f) && !f.contains('.') && f != "id" {
                Ok(())
            } else {
                Err(format!(
                    "data.{what}: `{f}` must be a plain field name (not `id`)"
                ))
            }
        };
        if let Some(o) = &self.owner {
            simple("owner", o)?;
        }
        if let Some(t) = &self.timestamps {
            for (k, f) in [
                ("created", &t.created),
                ("updated", &t.updated),
                ("deleted", &t.deleted),
            ] {
                if let Some(f) = f {
                    simple(&format!("timestamps.{k}"), f)?;
                }
            }
            if t.deleted.is_some() && self.soft_delete.is_none() {
                return Err("data.timestamps.deleted needs data.soft_delete".into());
            }
        }
        if let Some(f) = &self.soft_delete {
            simple("soft_delete", f)?;
        }
        for k in self.defaults.keys() {
            simple("defaults", k)?;
        }
        for f in self.readonly.iter().chain(&self.hide) {
            if !valid_field(f) || f == "id" {
                return Err(format!("data.readonly/hide: `{f}` is not a valid field path (and `id` cannot be listed)"));
            }
        }
        if let Some(a) = &self.audit {
            if !valid_collection(a) {
                return Err(format!("data.audit: `{a}` is not a valid collection name"));
            }
        }
        for (i, e) in self.effects.iter().enumerate() {
            let o = e
                .as_object()
                .ok_or_else(|| format!("data.effects[{i}] must be an object"))?;
            let coll = o
                .get("collection")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("data.effects[{i}] needs a literal `collection`"))?;
            if !valid_collection(coll) {
                return Err(format!("data.effects[{i}]: `{coll}` is not a valid collection name (templates are not allowed here)"));
            }
            match o.get("op").and_then(Value::as_str) {
                Some("insert" | "update" | "replace" | "upsert" | "delete") => {}
                _ => {
                    return Err(format!(
                        "data.effects[{i}] needs `op`: insert | update | replace | upsert | delete"
                    ))
                }
            }
        }
        Ok(())
    }

    fn owned(&self, f: &str) -> bool {
        self.readonly.iter().any(|r| r == f)
    }
}

// ------------------------------------------------------------------ time

/// `1700000000` -> `2023-11-14T22:13:20Z` (UTC, no external date crate).
pub fn iso8601(unix: i64) -> String {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

pub fn now_iso() -> String {
    iso8601(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    )
}

// ------------------------------------------------------------------ templates

fn lookup<'a>(vars: &'a Value, path: &str) -> Option<&'a Value> {
    get_path(vars, path)
}

fn eval(expr: &str, vars: &Value) -> Result<Value, String> {
    let expr = expr.trim();
    let (neg, path) = match expr.strip_prefix('-') {
        Some(p) => (true, p.trim()),
        None => (false, expr),
    };
    let v = lookup(vars, path).ok_or_else(|| {
        format!("`{path}` is not available in this request (a field the rule needs is missing)")
    })?;
    if !neg {
        return Ok(v.clone());
    }
    match v {
        Value::Number(n) if n.is_i64() => Ok(json!(-n.as_i64().unwrap_or_default())),
        Value::Number(n) => n
            .as_f64()
            .map(|f| json!(-f))
            .ok_or_else(|| format!("cannot negate `{path}`")),
        _ => Err(format!("cannot negate `{path}`: it is not a number")),
    }
}

/// Render `${…}` placeholders anywhere inside `v`.
pub fn render(v: &Value, vars: &Value) -> Result<Value, String> {
    match v {
        Value::String(s) => {
            if let Some(inner) = s
                .strip_prefix("${")
                .and_then(|r| r.strip_suffix('}'))
                .filter(|i| !i.contains("${"))
            {
                return eval(inner, vars);
            }
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(i) = rest.find("${") {
                out.push_str(&rest[..i]);
                let after = &rest[i + 2..];
                let end = after
                    .find('}')
                    .ok_or_else(|| format!("unterminated `${{` in `{s}`"))?;
                match eval(&after[..end], vars)? {
                    Value::String(t) => out.push_str(&t),
                    Value::Number(n) => out.push_str(&n.to_string()),
                    Value::Bool(b) => out.push_str(&b.to_string()),
                    other => {
                        return Err(format!(
                            "`{}` is {other}, which cannot be embedded in text",
                            &after[..end]
                        ))
                    }
                }
                rest = &after[end + 1..];
            }
            out.push_str(rest);
            Ok(Value::String(out))
        }
        Value::Array(a) => a
            .iter()
            .map(|x| render(x, vars))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(o) => o
            .iter()
            .map(|(k, x)| Ok((k.clone(), render(x, vars)?)))
            .collect::<Result<Map<_, _>, String>>()
            .map(Value::Object),
        other => Ok(other.clone()),
    }
}

// ------------------------------------------------------------------ applying rules

pub fn remove_field(doc: &mut Value, path: &str) {
    let segs: Vec<&str> = path.split('.').collect();
    let (last, parents) = segs.split_last().expect("one segment at least");
    let mut cur = doc;
    for s in parents {
        match cur.as_object_mut().and_then(|o| o.get_mut(*s)) {
            Some(n) => cur = n,
            None => return,
        }
    }
    if let Some(o) = cur.as_object_mut() {
        o.remove(*last);
    }
}

impl DataRules {
    /// Remove `hide`n fields from a response document.
    pub fn hide_in(&self, doc: &mut Value) {
        for f in &self.hide {
            remove_field(doc, f);
        }
    }

    /// Fields a client may never set directly: read-only fields, the owner, and the timestamps.
    pub fn protected(&self) -> Vec<String> {
        let mut p = self.readonly.clone();
        p.extend(self.owner.clone());
        if let Some(t) = &self.timestamps {
            p.extend(
                [t.created.clone(), t.updated.clone(), t.deleted.clone()]
                    .into_iter()
                    .flatten(),
            );
        }
        p.extend(self.soft_delete.clone());
        p
    }

    /// Body of a create: client-set protected fields dropped, then defaults, owner, stamps.
    pub fn shape_create(&self, body: &mut Map<String, Value>, subject: Option<&str>, now: &str) {
        for f in self.protected() {
            body.remove(&f);
        }
        for (k, v) in &self.defaults {
            body.entry(k.clone()).or_insert_with(|| v.clone());
        }
        // defaults may legitimately set read-only fields (they are server-side); they were added after stripping
        if let (Some(o), Some(s)) = (&self.owner, subject) {
            body.insert(o.clone(), json!(s));
        }
        if let Some(t) = &self.timestamps {
            for f in [&t.created, &t.updated].into_iter().flatten() {
                body.insert(f.clone(), json!(now));
            }
        }
        if let Some(f) = &self.soft_delete {
            body.insert(f.clone(), json!(false));
        }
    }

    /// Fields a replace must keep from the stored document.
    pub fn keep_on_replace(&self) -> Vec<String> {
        let mut k = self.protected();
        if let Some(t) = &self.timestamps {
            k.retain(|f| Some(f) != t.updated.as_ref()); // `updated` is re-stamped, not kept
        }
        k
    }

    /// Conditions every write to an existing document must satisfy (owner, not soft-deleted).
    pub fn guard(&self, subject: Option<&str>) -> Map<String, Value> {
        let mut m = Map::new();
        if let (Some(o), Some(s)) = (&self.owner, subject) {
            m.insert(o.clone(), json!(s));
        }
        if let Some(f) = &self.soft_delete {
            m.insert(f.clone(), json!({ "ne": true }));
        }
        m
    }

    pub fn is_stamped_updated(&self) -> Option<&str> {
        self.timestamps.as_ref().and_then(|t| t.updated.as_deref())
    }

    pub fn owned_field(&self, f: &str) -> bool {
        self.owned(f)
    }

    /// The route's templated effects, rendered for this request.
    pub fn render_effects(&self, vars: &Value) -> Result<Vec<Effect>, String> {
        self.effects
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let rendered = render(t, vars).map_err(|e| format!("data.effects[{i}]: {e}"))?;
                serde_json::from_value::<Effect>(rendered).map_err(|e| {
                    format!("data.effects[{i}]: not a valid effect after rendering: {e}")
                })
            })
            .collect()
    }

    /// The ledger row for a write.
    pub fn audit_effect(&self, vars: &Value, op: &str, collection: &str) -> Option<Effect> {
        let audit = self.audit.as_ref()?;
        let g = |p: &str| lookup(vars, p).cloned().unwrap_or(Value::Null);
        serde_json::from_value(json!({
            "op": "insert", "collection": audit,
            "doc": { "event": format!("{} {}", op, g("route").as_str().unwrap_or("")), "collection": collection, "docId": g("id"),
                     "by": g("identity.subject"), "at": g("now"), "requestId": g("request_id") }
        }))
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_matches_known_instants() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z", "leap day");
        assert_eq!(iso8601(4_102_444_799), "2099-12-31T23:59:59Z");
        assert_eq!(iso8601(-1), "1969-12-31T23:59:59Z");
        assert!(now_iso().starts_with("20") && now_iso().ends_with('Z'));
    }

    #[test]
    fn templates_keep_types_negate_and_interpolate() {
        let vars = json!({"body": {"qty": 3, "sku": "sku-1", "price": 2.5, "tags": ["a"], "n": null}, "identity": {"subject": "ann"}, "id": 7});
        assert_eq!(
            render(&json!("${body.qty}"), &vars).unwrap(),
            json!(3),
            "a whole-string placeholder keeps its type"
        );
        assert_eq!(render(&json!("${-body.qty}"), &vars).unwrap(), json!(-3));
        assert_eq!(
            render(&json!("${-body.price}"), &vars).unwrap(),
            json!(-2.5)
        );
        assert_eq!(
            render(
                &json!("order ${id} by ${identity.subject} x${body.qty}"),
                &vars
            )
            .unwrap(),
            json!("order 7 by ann x3")
        );
        assert_eq!(
            render(
                &json!({"a": ["${body.sku}", 1, {"b": "${body.tags}"}], "c": true}),
                &vars
            )
            .unwrap(),
            json!({"a": ["sku-1", 1, {"b": ["a"]}], "c": true})
        );
        assert!(render(&json!("${body.nope}"), &vars)
            .unwrap_err()
            .contains("body.nope"));
        assert!(render(&json!("${-body.sku}"), &vars)
            .unwrap_err()
            .contains("not a number"));
        assert!(render(&json!("x ${body.tags}"), &vars)
            .unwrap_err()
            .contains("cannot be embedded"));
        assert!(render(&json!("x ${body.qty"), &vars)
            .unwrap_err()
            .contains("unterminated"));
        assert_eq!(
            render(&json!("no placeholders"), &vars).unwrap(),
            json!("no placeholders")
        );
    }

    #[test]
    fn create_shaping_strips_protected_fields_then_applies_defaults_owner_and_stamps() {
        let rules = DataRules {
            owner: Some("ownerId".into()),
            timestamps: Some(Timestamps {
                created: Some("createdAt".into()),
                updated: Some("updatedAt".into()),
                deleted: None,
            }),
            defaults: serde_json::from_value(json!({"status": "open", "priority": 1})).unwrap(),
            readonly: vec!["status".into(), "total".into()],
            soft_delete: Some("deleted".into()),
            ..Default::default()
        };
        let mut body = json!({"name": "x", "status": "paid", "total": 999, "ownerId": "mallory", "createdAt": "FORGED", "deleted": true, "priority": 5}).as_object().unwrap().clone();
        rules.shape_create(&mut body, Some("ann"), "T1");
        assert_eq!(Value::Object(body), json!({"name": "x", "status": "open", "priority": 5, "ownerId": "ann", "createdAt": "T1", "updatedAt": "T1", "deleted": false}),
            "client cannot forge protected fields; defaults fill only what is missing; read-only fields get the server default");
        assert_eq!(
            rules.guard(Some("ann")),
            json!({"ownerId": "ann", "deleted": {"ne": true}})
                .as_object()
                .unwrap()
                .clone()
        );
        let keep = rules.keep_on_replace();
        assert!(
            keep.contains(&"createdAt".to_string())
                && keep.contains(&"ownerId".to_string())
                && keep.contains(&"status".to_string())
        );
        assert!(
            !keep.contains(&"updatedAt".to_string()),
            "updatedAt is re-stamped, not kept"
        );
    }

    #[test]
    fn hiding_and_effect_rendering() {
        let rules = DataRules {
            hide: vec!["secret".into(), "meta.internal".into()],
            audit: Some("audit".into()),
            effects: vec![
                json!({"op": "update", "collection": "products", "id": "${body.sku}", "inc": {"stock": "${-body.qty}"}, "require": {"stock": {"gte": "${body.qty}"}}}),
            ],
            ..Default::default()
        };
        let mut d = json!({"a": 1, "secret": 2, "meta": {"internal": 3, "ok": 4}});
        rules.hide_in(&mut d);
        assert_eq!(d, json!({"a": 1, "meta": {"ok": 4}}));
        let vars = json!({"body": {"sku": "sku-1", "qty": 2}, "id": "o1", "route": "POST /orders", "identity": {"subject": "ann"}, "now": "T", "request_id": "r1"});
        let fx = rules.render_effects(&vars).unwrap();
        assert_eq!(
            (
                fx[0].id.as_deref(),
                fx[0].inc.as_ref().unwrap()["stock"].clone()
            ),
            (Some("sku-1"), json!(-2))
        );
        let a = rules.audit_effect(&vars, "create", "orders").unwrap();
        assert_eq!(
            a.doc.unwrap(),
            json!({"event": "create POST /orders", "collection": "orders", "docId": "o1", "by": "ann", "at": "T", "requestId": "r1"})
        );
        assert!(rules
            .render_effects(&json!({"body": {}}))
            .unwrap_err()
            .contains("data.effects[0]"));
    }

    #[test]
    fn validation_rejects_bad_rules() {
        let bad = |v: Value| {
            serde_json::from_value::<DataRules>(v)
                .map_err(|e| e.to_string())
                .and_then(|r| r.validate().map(|_| r))
                .unwrap_err()
        };
        assert!(bad(json!({"owner": "a.b"})).contains("plain field"));
        assert!(bad(json!({"owner": "id"})).contains("plain field"));
        assert!(bad(json!({"timestamps": {"deleted": "d"}})).contains("soft_delete"));
        assert!(bad(json!({"hide": ["id"]})).contains("`id`"));
        assert!(bad(json!({"audit": "a b"})).contains("collection"));
        assert!(bad(json!({"effects": [5]})).contains("object"));
        assert!(
            bad(json!({"effects": [{"op": "update", "collection": "${x}"}]}))
                .contains("collection")
        );
        assert!(bad(json!({"effects": [{"op": "boom", "collection": "c"}]})).contains("`op`"));
        assert!(bad(json!({"ownr": "x"})).contains("unknown field"));
        assert!(serde_json::from_value::<DataRules>(json!({"owner": "ownerId", "soft_delete": "deleted", "timestamps": {"deleted": "deletedAt"}})).unwrap().validate().is_ok());
        assert!(DataRules::default().is_empty());
    }
}
