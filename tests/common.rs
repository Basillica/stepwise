#![allow(dead_code)]
use std::{net::SocketAddr, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use stepwise::{config::Config, hooks::HookRegistry, state::AppState};
use tower::ServiceExt;

pub fn spec_path() -> String {
    format!("{}/examples/api.yaml", env!("CARGO_MANIFEST_DIR"))
}

/// Minimal valid config pointing at the example spec; ports are 0 so tests never collide.
pub fn base_config() -> Config {
    Config::from_yaml_str(&format!(
        "control: {{token: ctl-token, port: 0}}\nserver: {{port: 0}}\nopenapi: {{spec_path: '{}'}}\nlogging: {{access_log: false}}\n",
        spec_path()
    ))
    .unwrap()
}

pub fn parse<T: DeserializeOwned>(yaml: &str) -> T {
    serde_yaml::from_str(yaml).unwrap()
}

pub fn state(cfg: Config) -> Arc<AppState> {
    AppState::new(cfg, None, HookRegistry::with_builtins(), None).unwrap()
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub text: String,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.get(k).and_then(|v| v.to_str().ok())
    }
}

pub async fn send_from(
    app: &Router,
    ip: [u8; 4],
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Resp {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut req = b
        .body(
            body.map(|s| Body::from(s.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((ip, 40000))));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = to_bytes(body, 10 * 1024 * 1024).await.unwrap();
    Resp {
        status: parts.status,
        headers: parts.headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

pub async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Resp {
    send_from(app, [127, 0, 0, 1], method, uri, headers, body).await
}

pub async fn send_raw(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Resp {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::from(body)).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = to_bytes(body, 10 * 1024 * 1024).await.unwrap();
    Resp {
        status: parts.status,
        headers: parts.headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

pub fn fixture(name: &str) -> Vec<u8> {
    let p = format!(
        "{}/tests/fixtures/prebuilt/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&p).unwrap_or_else(|e| panic!("missing fixture {p}: {e}"))
}

/// Optional big fixtures built on demand by `tests/fixtures/*/build.sh` into target/fixtures.
pub fn optional_fixture(name: &str) -> Option<Vec<u8>> {
    std::fs::read(format!(
        "{}/target/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .ok()
}
