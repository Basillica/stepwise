//! Client for the `stepwise-builder` service. Plain HTTP on purpose: the builder belongs on a
//! private network next to the server (TLS can be added by a proxy). Everything is bounded by
//! timeouts and a response-size cap, and the builder's own error JSON is surfaced intact so a UI
//! can show the compiler's message.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{HeaderMap, Method, Request};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};

use crate::config::BuilderConfig;

const MAX_RESPONSE: usize = 300 * 1024 * 1024;

pub struct BuilderClient {
    authority: String,
    token: String,
    timeout: Duration,
}

pub struct BuildOutput {
    pub wasm: Bytes,
    pub toolchain: String,
    pub log: String,
    pub build_ms: u64,
}

#[derive(Debug)]
pub enum BuilderError {
    /// Could not connect / transport failure.
    Unreachable(String),
    Timeout,
    /// The builder answered with an error (`status`, and its `{error:{code,message,log}}`).
    Rejected {
        status: u16,
        code: String,
        message: String,
        log: Option<String>,
    },
    Protocol(String),
}

impl BuilderClient {
    pub fn from_config(c: &BuilderConfig) -> Option<Self> {
        if c.url.is_empty() {
            return None;
        }
        Some(Self {
            authority: c
                .url
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string(),
            token: c.token.clone(),
            timeout: Duration::from_secs(c.timeout_secs),
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<(u16, HeaderMap, Bytes), BuilderError> {
        let req = Request::builder()
            .method(method)
            .uri(format!("http://{}{}", self.authority, path))
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|e| BuilderError::Protocol(e.to_string()))?;
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let fut = async {
            let resp = client
                .request(req)
                .await
                .map_err(|e| BuilderError::Unreachable(e.to_string()))?;
            let (parts, body) = resp.into_parts();
            let bytes = Limited::new(body, MAX_RESPONSE)
                .collect()
                .await
                .map_err(|e| BuilderError::Protocol(e.to_string()))?
                .to_bytes();
            Ok((parts.status.as_u16(), parts.headers, bytes))
        };
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| BuilderError::Timeout)?
    }

    fn rejected(status: u16, body: &[u8]) -> BuilderError {
        let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let e = &v["error"];
        BuilderError::Rejected {
            status,
            code: e["code"].as_str().unwrap_or("builder_error").to_string(),
            message: e["message"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(300).collect()),
            log: e["log"].as_str().map(String::from),
        }
    }

    pub async fn build(
        &self,
        language: &str,
        source: &str,
        backend: Option<&str>,
    ) -> Result<BuildOutput, BuilderError> {
        let mut body = json!({ "language": language, "source": source });
        if let Some(b) = backend {
            body["backend"] = json!(b);
        }
        let (status, headers, bytes) = self
            .request(
                Method::POST,
                "/v1/build",
                Some(body.to_string().into_bytes()),
                self.timeout,
            )
            .await?;
        if status != 200 {
            return Err(Self::rejected(status, &bytes));
        }
        let header = |k: &str| {
            headers
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(|v| {
                    percent_encoding::percent_decode_str(v)
                        .decode_utf8_lossy()
                        .into_owned()
                })
                .unwrap_or_default()
        };
        Ok(BuildOutput {
            wasm: bytes,
            toolchain: header("x-build-toolchain"),
            log: header("x-build-log"),
            build_ms: header("x-build-ms").parse().unwrap_or(0),
        })
    }

    /// GET a JSON endpoint of the builder (toolchains, templates, typings).
    pub async fn get_json(&self, path: &str) -> Result<Value, BuilderError> {
        let (status, _, bytes) = self
            .request(
                Method::GET,
                path,
                None,
                self.timeout.min(Duration::from_secs(30)),
            )
            .await?;
        if status != 200 {
            return Err(Self::rejected(status, &bytes));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| BuilderError::Protocol(format!("builder sent invalid JSON: {e}")))
    }
}
