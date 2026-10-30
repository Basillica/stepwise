//! Stepwise builder: compiles hook *source* (TypeScript, JavaScript, Python) into WebAssembly
//! components that satisfy `wit/stepwise-hook.wit`.
//!
//! It exists as its own service because it needs heavyweight toolchains (Node + `jco`,
//! `componentize-py`) that the Stepwise server should not carry. Run it on a private network
//! with no outbound access: building executes user-supplied code (inside Wasm, via Wizer
//! pre-initialisation), so treat the builder as the sandbox boundary and give it CPU/memory limits.

pub mod templates;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path as UrlPath, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{process::Command, sync::Semaphore};

const WIT_FILE: &str = "stepwise-hook.wit";
const WORLD: &str = "hook";
const MAX_LOG_CHARS: usize = 8000;
const MAX_WASM_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    /// Shared secret the Stepwise server presents (`Authorization: Bearer …`).
    pub token: String,
    pub jco: PathBuf,
    pub componentize_py: PathBuf,
    /// Directory containing `stepwise-hook.wit`.
    pub wit_dir: PathBuf,
    pub max_source_bytes: usize,
    pub js_timeout: Duration,
    pub py_timeout: Duration,
    /// Builds running at once; others wait briefly, then get 503.
    pub max_concurrent: usize,
}

impl Config {
    /// `BUILDER_TOKEN` is required; the rest have defaults suited to the Docker image.
    pub fn from_env() -> Result<Self> {
        let get = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let secs = |k: &str, d: u64| -> Result<Duration> {
            Ok(Duration::from_secs(
                get(k, &d.to_string())
                    .parse()
                    .with_context(|| format!("{k} must be a number of seconds"))?,
            ))
        };
        let token = std::env::var("BUILDER_TOKEN").context("BUILDER_TOKEN is required")?;
        if token.len() < 16 {
            anyhow::bail!("BUILDER_TOKEN must be at least 16 characters");
        }
        Ok(Self {
            token,
            jco: get("BUILDER_JCO", "jco").into(),
            componentize_py: get("BUILDER_COMPONENTIZE_PY", "componentize-py").into(),
            wit_dir: get("BUILDER_WIT_DIR", "/opt/stepwise/wit").into(),
            max_source_bytes: get("BUILDER_MAX_SOURCE_BYTES", "1048576")
                .parse()
                .context("BUILDER_MAX_SOURCE_BYTES")?,
            js_timeout: secs("BUILDER_JS_TIMEOUT_SECS", 90)?,
            py_timeout: secs("BUILDER_PY_TIMEOUT_SECS", 240)?,
            max_concurrent: get("BUILDER_MAX_CONCURRENT", "2")
                .parse()
                .context("BUILDER_MAX_CONCURRENT")?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Typescript,
    Javascript,
    Python,
}

impl Language {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "typescript" | "ts" => Some(Self::Typescript),
            "javascript" | "js" => Some(Self::Javascript),
            "python" | "py" => Some(Self::Python),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Typescript => "typescript",
            Self::Javascript => "javascript",
            Self::Python => "python",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// QuickJS: small (~1.7 MB), no WASI, fast start. Default.
    Qjs,
    /// SpiderMonkey: full modern JS, ~12 MB.
    Spidermonkey,
}

#[derive(Debug, Deserialize)]
pub struct BuildRequest {
    pub language: String,
    pub source: String,
    /// `qjs` (default) or `spidermonkey`; JS/TS only.
    pub backend: Option<String>,
}

#[derive(Debug)]
pub enum BuildError {
    Invalid(String),
    Unsupported(String),
    Failed { message: String, log: String },
    Timeout(Duration),
    Busy,
    Internal(String),
}

pub struct Built {
    pub wasm: Vec<u8>,
    pub log: String,
    pub duration: Duration,
    pub toolchain: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Toolchains {
    pub node: Option<String>,
    pub jco: Option<String>,
    pub componentize_py: Option<String>,
}

pub struct Builder {
    cfg: Config,
    permits: Semaphore,
    pub toolchains: Toolchains,
    typings: Option<BTreeMap<String, String>>,
}

/// `export const handler = …`, `export { handler }`, `export { x as handler }`, `export function handler`.
fn exports_handler(src: &str) -> bool {
    let mut rest = src;
    while let Some(i) = rest.find("export") {
        let after = &rest[i + 6..];
        let stmt: String = after
            .chars()
            .take_while(|c| *c != ';' && *c != '\n')
            .collect();
        let braces: String = if stmt.trim_start().starts_with('{') {
            after.chars().take_while(|c| *c != '}').collect()
        } else {
            String::new()
        };
        let hit = |t: &str| {
            t.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .any(|w| w == "handler")
        };
        if hit(&stmt) || hit(&braces) {
            return true;
        }
        rest = after;
    }
    false
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(n - max).collect::<String>())
    }
}

/// Run a toolchain command with a clean environment and a hard timeout. Returns (success, combined output).
async fn run(
    mut cmd: Command,
    dir: &Path,
    timeout: Duration,
) -> Result<(bool, String), BuildError> {
    // Scratch space must live outside the project directory: componentize-js uses TMPDIR for its
    // own work files and resolves the entry module against the project, and nesting them collides.
    let scratch = tempfile::Builder::new()
        .prefix("stepwise-scratch-")
        .tempdir()
        .map_err(|e| BuildError::Internal(e.to_string()))?;
    cmd.current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", scratch.path())
        .env("TMPDIR", scratch.path())
        .env("NODE_OPTIONS", "--max-old-space-size=1536")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| BuildError::Internal(format!("cannot start toolchain: {e}")))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Err(BuildError::Timeout(timeout)), // child dropped => killed
        Ok(Err(e)) => Err(BuildError::Internal(e.to_string())),
        Ok(Ok(out)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            Ok((
                out.status.success(),
                tail(&strip_ansi(&text), MAX_LOG_CHARS),
            ))
        }
    }
}

async fn probe(bin: &Path, arg: &str) -> Option<String> {
    let dir = std::env::temp_dir();
    let mut c = Command::new(bin);
    c.arg(arg);
    match run(c, &dir, Duration::from_secs(30)).await {
        Ok((true, out)) => out.lines().next().map(|l| l.trim().to_string()),
        _ => None,
    }
}

impl Builder {
    pub async fn new(cfg: Config) -> Result<Arc<Self>> {
        if !cfg.wit_dir.join(WIT_FILE).exists() {
            anyhow::bail!(
                "{} not found in BUILDER_WIT_DIR ({})",
                WIT_FILE,
                cfg.wit_dir.display()
            );
        }
        let node = probe(Path::new("node"), "--version").await;
        let jco = probe(&cfg.jco, "--version").await;
        let cpy = probe(&cfg.componentize_py, "--version").await;
        let typings = if jco.is_some() {
            Self::generate_typings(&cfg).await.ok()
        } else {
            None
        };
        Ok(Arc::new(Self {
            permits: Semaphore::new(cfg.max_concurrent.max(1)),
            toolchains: Toolchains {
                node,
                jco,
                componentize_py: cpy,
            },
            typings,
            cfg,
        }))
    }

    async fn generate_typings(cfg: &Config) -> Result<BTreeMap<String, String>> {
        let dir = tempfile::tempdir()?;
        let mut c = Command::new(&cfg.jco);
        c.args(["types"])
            .arg(&cfg.wit_dir)
            .args(["--world-name", WORLD, "-o"])
            .arg(dir.path().join("types"));
        match run(c, dir.path(), Duration::from_secs(60)).await {
            Ok((true, _)) => {}
            other => return Err(anyhow!("jco types failed: {other:?}")),
        }
        let mut files = BTreeMap::new();
        let root = dir.path().join("types");
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)? {
                let p = e?.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "ts") {
                    files.insert(
                        p.strip_prefix(&root)?.to_string_lossy().into_owned(),
                        std::fs::read_to_string(&p)?,
                    );
                }
            }
        }
        Ok(files)
    }

    pub fn supports(&self, l: Language) -> bool {
        match l {
            Language::Typescript | Language::Javascript => self.toolchains.jco.is_some(),
            Language::Python => self.toolchains.componentize_py.is_some(),
        }
    }

    pub async fn build(&self, req: &BuildRequest) -> Result<Built, BuildError> {
        let lang = Language::parse(&req.language).ok_or_else(|| {
            BuildError::Invalid(format!(
                "language must be typescript, javascript or python, not `{}`",
                req.language
            ))
        })?;
        if req.source.len() > self.cfg.max_source_bytes {
            return Err(BuildError::Invalid(format!(
                "source is {} bytes, over the {} byte limit",
                req.source.len(),
                self.cfg.max_source_bytes
            )));
        }
        if req.source.trim().is_empty() || req.source.contains('\0') {
            return Err(BuildError::Invalid(
                "source is empty or contains NUL bytes".into(),
            ));
        }
        let backend = match (req.backend.as_deref(), lang) {
            (None | Some("qjs"), _) => Backend::Qjs,
            (Some("spidermonkey"), Language::Typescript | Language::Javascript) => {
                Backend::Spidermonkey
            }
            (Some(b), _) => {
                return Err(BuildError::Invalid(format!(
                    "backend `{b}` is not valid for {} (use qjs or spidermonkey for JS/TS)",
                    lang.name()
                )))
            }
        };
        // The toolchains happily build a module that does not implement the hook interface; the
        // failure would only show up on the first request. Catch the common mistake here.
        match lang {
            Language::Typescript | Language::Javascript if !exports_handler(&req.source) => {
                return Err(BuildError::Failed {
                    message: "the source does not export `handler`".into(),
                    log: "A hook must export an object named `handler` with a `handle(ctx)` method, e.g.\n\n  export const handler = {\n    handle(ctx) { return { tag: \"proceed\", val: {} }; },\n  };".into(),
                })
            }
            Language::Python if !req.source.contains("exports.Handler") && !req.source.contains("Handler") => {
                return Err(BuildError::Failed {
                    message: "the source does not define a `Handler` class".into(),
                    log: "A hook must define `class Handler(exports.Handler)` with a `handle(self, ctx)` method (see the Python template).".into(),
                })
            }
            _ => {}
        }
        if !self.supports(lang) {
            return Err(BuildError::Unsupported(format!(
                "this builder has no toolchain for {}",
                lang.name()
            )));
        }

        let _permit =
            match tokio::time::timeout(Duration::from_secs(60), self.permits.acquire()).await {
                Ok(Ok(p)) => p,
                _ => return Err(BuildError::Busy),
            };
        let started = Instant::now();
        let dir = tempfile::tempdir().map_err(|e| BuildError::Internal(e.to_string()))?;
        let out = dir.path().join("out.wasm");

        let (mut cmd, timeout, toolchain) = match lang {
            Language::Typescript | Language::Javascript => {
                let file = if lang == Language::Typescript {
                    "app.ts"
                } else {
                    "app.js"
                };
                std::fs::write(dir.path().join(file), &req.source)
                    .map_err(|e| BuildError::Internal(e.to_string()))?;
                let mut c = Command::new(&self.cfg.jco);
                // SpiderMonkey resolves a bare `app.js` as `./index.js`; the explicit `./` works for both backends.
                c.args(["componentize", &format!("./{file}"), "--wit"])
                    .arg(&self.cfg.wit_dir)
                    .args(["--world-name", WORLD]);
                match backend {
                    // QuickJS: smallest artifact; no WASI imports at all.
                    Backend::Qjs => c.args([
                        "--backend",
                        "qjs",
                        "--backend-qjs-disable-async",
                        "--backend-qjs-stub-wasi",
                    ]),
                    // The host provides no wasi:http, so build without it.
                    Backend::Spidermonkey => {
                        c.args(["--disable", "http", "--disable", "fetch-event"])
                    }
                };
                c.arg("-o").arg(&out);
                let tc = format!(
                    "jco {} ({})",
                    self.toolchains.jco.clone().unwrap_or_default(),
                    if backend == Backend::Qjs {
                        "qjs"
                    } else {
                        "spidermonkey"
                    }
                );
                (c, self.cfg.js_timeout, tc)
            }
            Language::Python => {
                std::fs::write(dir.path().join("app.py"), &req.source)
                    .map_err(|e| BuildError::Internal(e.to_string()))?;
                let mut c = Command::new(&self.cfg.componentize_py);
                c.arg("-d")
                    .arg(&self.cfg.wit_dir)
                    .args(["-w", WORLD, "componentize", "app", "-o"])
                    .arg(&out);
                (
                    c,
                    self.cfg.py_timeout,
                    format!(
                        "componentize-py {}",
                        self.toolchains.componentize_py.clone().unwrap_or_default()
                    ),
                )
            }
        };
        let (ok, log) = run(
            std::mem::replace(&mut cmd, Command::new("true")),
            dir.path(),
            timeout,
        )
        .await?;
        if !ok {
            return Err(BuildError::Failed {
                message: "the compiler rejected the source".into(),
                log,
            });
        }
        let meta = std::fs::metadata(&out).map_err(|_| BuildError::Failed {
            message: "the toolchain produced no output".into(),
            log: log.clone(),
        })?;
        if meta.len() > MAX_WASM_BYTES {
            return Err(BuildError::Failed {
                message: format!("component is {} bytes, over the limit", meta.len()),
                log,
            });
        }
        let wasm = std::fs::read(&out).map_err(|e| BuildError::Internal(e.to_string()))?;
        if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
            return Err(BuildError::Failed {
                message: "output is not WebAssembly".into(),
                log,
            });
        }
        Ok(Built {
            wasm,
            log,
            duration: started.elapsed(),
            toolchain,
        })
    }

    pub fn wit_text(&self) -> Result<String> {
        std::fs::read_to_string(self.cfg.wit_dir.join(WIT_FILE)).context("reading WIT")
    }
}

// ------------------------------------------------------------------ HTTP

fn err(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
    log: Option<String>,
) -> Response {
    let mut e = json!({ "code": code, "message": message.into() });
    if let Some(l) = log {
        e["log"] = Value::String(l);
    }
    (status, Json(json!({ "error": e }))).into_response()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_token(State(b): State<Arc<Builder>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/v1/health" {
        return next.run(req).await;
    }
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.get(..7)
                .filter(|p| p.eq_ignore_ascii_case("bearer "))
                .map(|_| &v[7..])
        })
        .is_some_and(|t| ct_eq(t.trim().as_bytes(), b.cfg.token.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        err(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid builder token",
            None,
        )
    }
}

pub fn router(b: Arc<Builder>) -> Router {
    let limit = b.cfg.max_source_bytes + 64 * 1024;
    Router::new()
        .route(
            "/v1/health",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .route("/v1/toolchains", get(toolchains))
        .route("/v1/templates/:language", get(template))
        .route("/v1/wit", get(wit))
        .route("/v1/typings", get(typings))
        .route("/v1/build", post(build))
        .layer(DefaultBodyLimit::max(limit))
        .layer(middleware::from_fn_with_state(b.clone(), require_token))
        .with_state(b)
}

async fn toolchains(State(b): State<Arc<Builder>>) -> Json<Value> {
    let languages: Vec<&str> = [Language::Typescript, Language::Javascript, Language::Python]
        .into_iter()
        .filter(|l| b.supports(*l))
        .map(Language::name)
        .collect();
    Json(json!({
        "toolchains": b.toolchains,
        "languages": languages,
        "backends": { "javascript": ["qjs", "spidermonkey"], "typescript": ["qjs", "spidermonkey"] },
        "max_source_bytes": b.cfg.max_source_bytes,
    }))
}

async fn template(UrlPath(language): UrlPath<String>) -> Response {
    let Some(l) = Language::parse(&language) else {
        return err(StatusCode::NOT_FOUND, "not_found", "unknown language", None);
    };
    let (filename, source) = match l {
        Language::Typescript => ("app.ts", templates::TYPESCRIPT),
        Language::Javascript => ("app.js", templates::JAVASCRIPT),
        Language::Python => ("app.py", templates::PYTHON),
    };
    Json(json!({ "language": l.name(), "filename": filename, "source": source })).into_response()
}

async fn wit(State(b): State<Arc<Builder>>) -> Response {
    match b.wit_text() {
        Ok(t) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], t).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("{e:#}"),
            None,
        ),
    }
}

async fn typings(State(b): State<Arc<Builder>>) -> Response {
    match &b.typings {
        Some(files) => Json(json!({ "files": files })).into_response(),
        None => err(
            StatusCode::NOT_FOUND,
            "not_available",
            "typings could not be generated (jco is missing)",
            None,
        ),
    }
}

async fn build(State(b): State<Arc<Builder>>, body: axum::body::Bytes) -> Response {
    let req: BuildRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("body must be {{language, source, backend?}}: {e}"),
                None,
            )
        }
    };
    match b.build(&req).await {
        Ok(built) => {
            let mut resp = Response::new(Body::from(built.wasm));
            let h = resp.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/wasm"),
            );
            h.insert(
                "x-build-ms",
                HeaderValue::from(built.duration.as_millis() as u64),
            );
            for (k, v) in [
                ("x-build-toolchain", built.toolchain),
                ("x-build-log", tail(&built.log, 2000)),
            ] {
                if let Ok(val) =
                    HeaderValue::from_str(&utf8_percent_encode(&v, NON_ALPHANUMERIC).to_string())
                {
                    h.insert(k, val);
                }
            }
            resp
        }
        Err(BuildError::Invalid(m)) => err(StatusCode::BAD_REQUEST, "invalid_request", m, None),
        Err(BuildError::Unsupported(m)) => {
            err(StatusCode::NOT_IMPLEMENTED, "unsupported_language", m, None)
        }
        Err(BuildError::Failed { message, log }) => err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "build_failed",
            message,
            Some(log),
        ),
        Err(BuildError::Timeout(d)) => err(
            StatusCode::GATEWAY_TIMEOUT,
            "build_timeout",
            format!("the build exceeded {} seconds and was stopped", d.as_secs()),
            None,
        ),
        Err(BuildError::Busy) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "builder_busy",
            "too many builds in progress; retry shortly",
            None,
        ),
        Err(BuildError::Internal(m)) => {
            tracing::error!(error = %m, "internal build error");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "the build failed unexpectedly",
                None,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::exports_handler;

    #[test]
    fn detects_handler_exports() {
        assert!(exports_handler("export const handler = { handle() {} };"));
        assert!(exports_handler("const handler = {};\nexport { handler };"));
        assert!(exports_handler("export { impl as handler, other };"));
        assert!(exports_handler("export function handler() {}"));
        assert!(!exports_handler("export const nothing = 1;"));
        assert!(!exports_handler("const handler = {}; // not exported"));
        assert!(
            !exports_handler("export const handlers = {};"),
            "a longer name is not `handler`"
        );
    }
}
