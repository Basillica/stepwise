//! Work out *what changed* between two configs and *what that requires*:
//! hot-swap the runtime, gracefully restart a listener, or restart the process.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use crate::config::Config;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// New runtime is swapped in atomically; in-flight requests finish on the old one.
    Hot,
    /// Data-plane listener is gracefully drained and re-bound (new TLS / address / CORS).
    RestartDataListener,
    /// Control-plane listener is gracefully drained and re-bound.
    RestartControlListener,
    /// Saved, but only takes effect after the process restarts (e.g. log format).
    RestartProcess,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub action: Action,
}

/// Longest matching prefix wins; anything unlisted is hot.
const RULES: &[(&str, Action)] = &[
    ("server.host", Action::RestartDataListener),
    ("server.port", Action::RestartDataListener),
    ("server.tls", Action::RestartDataListener),
    ("server.cors", Action::RestartDataListener),
    ("control.enabled", Action::RestartControlListener),
    ("control.host", Action::RestartControlListener),
    ("control.port", Action::RestartControlListener),
    ("control.tls", Action::RestartControlListener),
    ("database", Action::RestartProcess),
    ("wasm.enabled", Action::RestartProcess),
    ("wasm.data_dir", Action::RestartProcess),
    ("wasm.max_memory_mb", Action::RestartProcess),
    ("wasm.max_concurrent", Action::RestartProcess),
    ("wasm.allow_wasi", Action::RestartProcess),
    ("wasm.max_upload_mb", Action::RestartControlListener),
    ("logging.format", Action::RestartProcess),
    ("logging.file", Action::RestartProcess),
];

pub fn classify(path: &str) -> Action {
    RULES
        .iter()
        .filter(|(p, _)| path == *p || path.starts_with(&format!("{p}.")))
        .max_by_key(|(p, _)| p.len())
        .map(|(_, a)| *a)
        .unwrap_or(Action::Hot)
}

/// Leaf paths (as key segments) where `a` and `b` differ. Arrays are compared whole.
pub fn leaf_diff(a: &Value, b: &Value) -> Vec<Vec<String>> {
    fn walk(a: &Value, b: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
                for k in keys {
                    path.push(k.clone());
                    match (x.get(k), y.get(k)) {
                        (Some(p), Some(q)) => walk(p, q, path, out),
                        _ => out.push(path.clone()),
                    }
                    path.pop();
                }
            }
            _ if a != b => out.push(path.clone()),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(a, b, &mut Vec::new(), &mut out);
    out
}

pub fn diff(old: &Config, new: &Config) -> Vec<Change> {
    let (a, b) = (to_json(old), to_json(new));
    let mut changes: Vec<Change> = leaf_diff(&a, &b)
        .into_iter()
        .map(|p| {
            let path = p.join(".");
            Change {
                action: classify(&path),
                path,
            }
        })
        .collect();
    changes.sort_by(|x, y| x.path.cmp(&y.path));
    changes
}

pub fn to_json(c: &Config) -> Value {
    serde_json::to_value(c).unwrap_or(Value::Null)
}

/// Apply `changed` leaf paths from `incoming` onto the raw (pre-expansion) YAML so that
/// everything else in the user's file, including `${ENV}` placeholders, stays untouched.
pub fn patch_raw(raw: &mut serde_yaml::Value, changed: &[Vec<String>], incoming: &Value) {
    for path in changed {
        let new = path.iter().try_fold(incoming, |v, k| v.get(k));
        set_path(raw, path, new.and_then(|v| serde_yaml::to_value(v).ok()));
    }
}

fn set_path(root: &mut serde_yaml::Value, path: &[String], value: Option<serde_yaml::Value>) {
    use serde_yaml::{Mapping, Value as Y};
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = root;
    for k in parents {
        if !cur.is_mapping() {
            if value.is_none() {
                return;
            }
            *cur = Y::Mapping(Mapping::new());
        }
        let m = cur.as_mapping_mut().expect("just ensured mapping");
        let key = Y::String(k.clone());
        if !m.contains_key(&key) {
            if value.is_none() {
                return;
            }
            m.insert(key.clone(), Y::Mapping(Mapping::new()));
        }
        cur = m.get_mut(&key).expect("just inserted");
    }
    if !cur.is_mapping() {
        if value.is_none() {
            return;
        }
        *cur = Y::Mapping(Mapping::new());
    }
    let m = cur.as_mapping_mut().expect("mapping");
    match value {
        Some(v) => {
            m.insert(Y::String(last.clone()), v);
        }
        None => {
            m.remove(Y::String(last.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(yaml: &str) -> Config {
        Config::from_yaml_str(&format!("control: {{token: t}}\n{yaml}")).unwrap()
    }

    #[test]
    fn classification() {
        assert_eq!(classify("server.port"), Action::RestartDataListener);
        assert_eq!(
            classify("server.tls.cert_path"),
            Action::RestartDataListener
        );
        assert_eq!(classify("server.max_body_bytes"), Action::Hot);
        assert_eq!(classify("control.token"), Action::Hot);
        assert_eq!(classify("control.port"), Action::RestartControlListener);
        assert_eq!(classify("logging.level"), Action::Hot);
        assert_eq!(classify("logging.format"), Action::RestartProcess);
        assert_eq!(classify("routes.GET /pets.rate_limit"), Action::Hot);
    }

    #[test]
    fn diff_reports_leaf_changes_with_actions() {
        let a = cfg("server: {port: 8080}\nlogging: {level: info}");
        let b = cfg("server: {port: 9000}\nlogging: {level: debug, format: json}");
        let d = diff(&a, &b);
        let get = |p: &str| d.iter().find(|c| c.path == p).map(|c| c.action);
        assert_eq!(get("server.port"), Some(Action::RestartDataListener));
        assert_eq!(get("logging.level"), Some(Action::Hot));
        assert_eq!(get("logging.format"), Some(Action::RestartProcess));
        assert!(diff(&a, &a).is_empty());
    }

    #[test]
    fn patching_raw_yaml_preserves_placeholders() {
        let mut raw: serde_yaml::Value = serde_yaml::from_str(
            "control:\n  token: ${TOKEN}\nserver:\n  port: 8080\nroutes:\n  a: {auth: true}\n",
        )
        .unwrap();
        let old = json!({"server":{"port":8080},"routes":{"a":{"auth":true}}});
        let new = json!({"server":{"port":9000},"routes":{"b":{"auth":false}}});
        patch_raw(&mut raw, &leaf_diff(&old, &new), &new);
        let out = serde_yaml::to_string(&raw).unwrap();
        assert!(out.contains("${TOKEN}"), "{out}");
        assert!(out.contains("9000"));
        assert!(
            !out.contains("\n  a:"),
            "removed route should be gone: {out}"
        );
        assert!(out.contains("b:"));
    }
}
