//! OpenAPI loading (JSON or YAML) and `$ref` inlining.
//!
//! We deliberately work on `serde_json::Value` rather than a typed OpenAPI model:
//! it handles 3.0 and 3.1 alike and keeps vendor extensions intact.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

pub fn load_spec(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read OpenAPI spec {}", path.display()))?;
    let is_json = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        || text.trim_start().starts_with('{');
    let value: Value = if is_json {
        serde_json::from_str(&text).context("spec is not valid JSON")?
    } else {
        serde_yaml::from_str(&text).context("spec is not valid YAML")?
    };
    if value.get("openapi").is_none() {
        anyhow::bail!(
            "{} does not look like an OpenAPI 3.x document (no `openapi` key)",
            path.display()
        );
    }
    Ok(value)
}

/// Inlines internal `$ref`s. Cycles collapse to `{}` (= "any") instead of recursing forever.
pub struct Resolver<'a> {
    root: &'a Value,
}

impl<'a> Resolver<'a> {
    pub fn new(root: &'a Value) -> Self {
        Self { root }
    }

    pub fn resolve(&self, v: &Value) -> Value {
        self.walk(v, &mut Vec::new(), 0)
    }

    fn walk(&self, v: &Value, stack: &mut Vec<String>, depth: usize) -> Value {
        match v {
            Value::Object(map) => {
                if let Some(Value::String(r)) = map.get("$ref") {
                    if depth > 64 || stack.contains(r) {
                        return Value::Object(Map::new());
                    }
                    let Some(target) = r.strip_prefix('#').and_then(|p| self.root.pointer(p))
                    else {
                        tracing::warn!(reference = %r, "unresolvable $ref (only internal refs are supported)");
                        return Value::Object(Map::new());
                    };
                    stack.push(r.clone());
                    let out = self.walk(target, stack, depth + 1);
                    stack.pop();
                    return out;
                }
                let mut out = Map::with_capacity(map.len());
                for (k, val) in map {
                    out.insert(k.clone(), self.walk(val, stack, depth + 1));
                }
                normalize_nullable(&mut out);
                Value::Object(out)
            }
            Value::Array(a) => {
                Value::Array(a.iter().map(|x| self.walk(x, stack, depth + 1)).collect())
            }
            other => other.clone(),
        }
    }
}

/// OpenAPI 3.0 `nullable: true` -> JSON Schema `type: [T, "null"]`.
fn normalize_nullable(obj: &mut Map<String, Value>) {
    if obj.get("nullable") != Some(&Value::Bool(true)) {
        return;
    }
    if let Some(Value::String(t)) = obj.get("type").cloned() {
        obj.insert(
            "type".into(),
            Value::Array(vec![Value::String(t), Value::String("null".into())]),
        );
        obj.remove("nullable");
    }
}
