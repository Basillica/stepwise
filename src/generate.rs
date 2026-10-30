//! Deterministic example generation from a (ref-resolved) JSON Schema.
//! Precedence: example > const > default > enum[0] > synthesized from type/format.

use serde_json::{Map, Number, Value};

#[derive(Debug, Clone)]
pub struct GenOptions {
    pub array_items: usize,
    pub max_depth: usize,
}

impl Default for GenOptions {
    fn default() -> Self {
        Self {
            array_items: 1,
            max_depth: 8,
        }
    }
}

pub fn generate(schema: &Value, opts: &GenOptions) -> Value {
    gen(schema, opts, 0)
}

fn gen(schema: &Value, o: &GenOptions, depth: usize) -> Value {
    let Some(obj) = schema.as_object() else {
        return Value::Null;
    };
    if depth > o.max_depth {
        return Value::Null;
    }
    if let Some(v) = obj.get("example") {
        return v.clone();
    }
    if let Some(Value::Array(ex)) = obj.get("examples") {
        if let Some(v) = ex.first() {
            return v.clone();
        }
    }
    for key in ["const", "default"] {
        if let Some(v) = obj.get(key) {
            return v.clone();
        }
    }
    if let Some(Value::Array(e)) = obj.get("enum") {
        if let Some(v) = e.first() {
            return v.clone();
        }
    }
    if let Some(Value::Array(subs)) = obj.get("allOf") {
        return gen(&Value::Object(merge_all_of(obj, subs)), o, depth + 1);
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(subs)) = obj.get(key) {
            if let Some(first) = subs.first() {
                return gen(first, o, depth + 1);
            }
        }
    }
    match schema_type(obj) {
        "object" => gen_object(obj, o, depth),
        "array" => gen_array(obj, o, depth),
        "string" => gen_string(obj),
        "integer" => gen_number(obj, true),
        "number" => gen_number(obj, false),
        "boolean" => Value::Bool(true),
        _ => Value::Null,
    }
}

fn schema_type(obj: &Map<String, Value>) -> &str {
    match obj.get("type") {
        Some(Value::String(s)) => s,
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null")
            .unwrap_or("null"),
        _ if obj.contains_key("properties") => "object",
        _ if obj.contains_key("items") => "array",
        _ => "any",
    }
}

fn merge_all_of(base: &Map<String, Value>, subs: &[Value]) -> Map<String, Value> {
    let mut merged: Map<String, Value> = base
        .iter()
        .filter(|(k, _)| k.as_str() != "allOf")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for sub in subs {
        let Some(so) = sub.as_object() else { continue };
        let so = match so.get("allOf") {
            Some(Value::Array(nested)) => merge_all_of(so, nested),
            _ => so.clone(),
        };
        for (k, v) in so {
            match (merged.get_mut(&k), &v) {
                (Some(Value::Object(dst)), Value::Object(src)) if k == "properties" => {
                    for (pk, pv) in src {
                        dst.entry(pk.clone()).or_insert_with(|| pv.clone());
                    }
                }
                (Some(Value::Array(dst)), Value::Array(src)) if k == "required" => {
                    dst.extend(src.iter().cloned());
                }
                (Some(_), _) => {} // first definition wins
                (None, _) => {
                    merged.insert(k, v);
                }
            }
        }
    }
    merged
}

fn gen_object(obj: &Map<String, Value>, o: &GenOptions, depth: usize) -> Value {
    let mut out = Map::new();
    if let Some(Value::Object(props)) = obj.get("properties") {
        for (k, v) in props {
            out.insert(k.clone(), gen(v, o, depth + 1));
        }
    }
    Value::Object(out)
}

fn gen_array(obj: &Map<String, Value>, o: &GenOptions, depth: usize) -> Value {
    let Some(items) = obj.get("items") else {
        return Value::Array(vec![]);
    };
    let min = obj.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
    let max = obj
        .get("maxItems")
        .and_then(Value::as_u64)
        .map(|m| m as usize);
    let mut n = o.array_items.max(min);
    if let Some(m) = max {
        n = n.min(m);
    }
    Value::Array((0..n).map(|_| gen(items, o, depth + 1)).collect())
}

fn gen_string(obj: &Map<String, Value>) -> Value {
    let base = match obj.get("format").and_then(Value::as_str) {
        Some("date-time") => "2024-01-01T00:00:00Z",
        Some("date") => "2024-01-01",
        Some("time") => "00:00:00Z",
        Some("uuid") => "00000000-0000-4000-8000-000000000000",
        Some("email") => "user@example.com",
        Some("uri" | "url") => "https://example.com",
        Some("hostname") => "example.com",
        Some("ipv4") => "192.0.2.1",
        Some("ipv6") => "2001:db8::1",
        Some("byte") => "c3RyaW5n",
        _ => "string",
    };
    let mut s = base.to_string();
    if let Some(min) = obj.get("minLength").and_then(Value::as_u64) {
        while (s.chars().count() as u64) < min {
            s.push('a');
        }
    }
    if let Some(max) = obj.get("maxLength").and_then(Value::as_u64) {
        s = s.chars().take(max as usize).collect();
    }
    Value::String(s)
}

fn gen_number(obj: &Map<String, Value>, integer: bool) -> Value {
    let f = |k: &str| obj.get(k).and_then(Value::as_f64);
    let mut v = 0.0_f64;
    if let Some(m) = f("minimum") {
        if v < m {
            v = m;
        }
        if obj.get("exclusiveMinimum") == Some(&Value::Bool(true)) && v <= m {
            v = m + 1.0;
        }
    }
    if let Some(m) = f("exclusiveMinimum") {
        // OpenAPI 3.1 / JSON Schema numeric form
        if v <= m {
            v = m + 1.0;
        }
    }
    if let Some(m) = f("maximum") {
        if v > m {
            v = m;
        }
    }
    if let Some(m) = f("multipleOf").filter(|m| *m > 0.0) {
        v = (v / m).ceil() * m;
    }
    if integer {
        Value::from(v as i64)
    } else {
        Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn g(s: Value) -> Value {
        generate(&s, &GenOptions::default())
    }

    #[test]
    fn precedence_example_over_type() {
        assert_eq!(
            g(json!({"type":"string","example":"hi","default":"x"})),
            json!("hi")
        );
        assert_eq!(g(json!({"type":"string","default":"x"})), json!("x"));
        assert_eq!(g(json!({"type":"string","enum":["a","b"]})), json!("a"));
    }

    #[test]
    fn formats_and_bounds() {
        assert_eq!(
            g(json!({"type":"string","format":"date"})),
            json!("2024-01-01")
        );
        assert_eq!(g(json!({"type":"string","minLength":8})), json!("stringaa"));
        assert_eq!(g(json!({"type":"string","maxLength":3})), json!("str"));
        assert_eq!(g(json!({"type":"integer","minimum":5})), json!(5));
        assert_eq!(g(json!({"type":"integer","maximum":-2})), json!(-2));
        assert_eq!(
            g(json!({"type":"integer","minimum":1,"exclusiveMinimum":true})),
            json!(2)
        );
        assert_eq!(g(json!({"type":["string","null"]})), json!("string"));
    }

    #[test]
    fn objects_arrays_all_of() {
        let s = json!({
            "allOf": [
                {"type":"object","properties":{"name":{"type":"string"}},"required":["name"]},
                {"type":"object","properties":{"id":{"type":"integer"}}}
            ]
        });
        assert_eq!(g(s), json!({"name":"string","id":0}));
        let arr = json!({"type":"array","items":{"type":"boolean"},"minItems":2});
        assert_eq!(g(arr), json!([true, true]));
        let opts = GenOptions {
            array_items: 3,
            ..Default::default()
        };
        assert_eq!(
            generate(
                &json!({"type":"array","items":{"type":"integer"},"maxItems":2}),
                &opts
            ),
            json!([0, 0])
        );
    }

    #[test]
    fn depth_limit_stops_runaway() {
        let mut s = json!({"type":"string"});
        for _ in 0..30 {
            s = json!({"type":"object","properties":{"c": s}});
        }
        let _ = g(s); // must terminate
    }
}
