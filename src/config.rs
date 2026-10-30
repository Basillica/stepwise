//! YAML configuration. Layout stays compatible with Mockmeister (`server`, `openapi`,
//! `auth`, `ignore_endpoints`) and adds per-route `routes`, `rate_limit`, `security`,
//! `control` (the control-plane listener) and `logging`.
//!
//! `${VAR}` / `${VAR:-default}` in the YAML text is expanded from the environment, so
//! secrets never have to live in the file.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::pipeline::Step;

pub const REDACTED: &str = "***";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub app_name: String,
    pub version: String,
    pub server: ServerConfig,
    pub control: ControlConfig,
    pub openapi: OpenApiConfig,
    pub auth: AuthConfig,
    pub rate_limit: RateLimitConfig,
    pub security: SecurityConfig,
    pub logging: LoggingConfig,
    pub wasm: WasmConfig,
    pub database: DatabaseConfig,
    /// Named relations for the read language: `collection -> name -> { from, by | id }`.
    pub relations: crate::db::query::Relations,
    /// Regexes matched against OpenAPI path templates; matching paths are not served.
    pub ignore_endpoints: Vec<String>,
    /// Per-route overrides, keyed `"METHOD /path/{param}"`.
    pub routes: BTreeMap<String, RouteConfig>,
    /// Sections not implemented yet (database, websocket, ...). Warned about at startup.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

// ---------------------------------------------------------------- server / tls / control

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub cors: CorsConfig,
    pub tls: TlsConfig,
    /// Take the client IP from `X-Forwarded-For` (only enable behind a trusted proxy!).
    pub trust_forwarded_for: bool,
    pub request_timeout_secs: u64,
    pub max_body_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            cors: CorsConfig::default(),
            tls: TlsConfig::default(),
            trust_forwarded_for: false,
            request_timeout_secs: 30,
            max_body_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TlsConfig {
    pub enabled: bool,
    pub cert_path: String,
    pub key_path: String,
    /// PEM bundle of CAs allowed to sign client certificates. Setting it turns on mTLS.
    pub client_ca_path: Option<String>,
    /// With `client_ca_path`: reject clients without a valid certificate (default true).
    /// `false` = optional client auth.
    pub require_client_cert: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// Bearer token for the control API. Required. Use `${STEPWISE_CONTROL_TOKEN}`.
    pub token: String,
    pub tls: TlsConfig,
    /// Persist applied config changes back to the config file.
    pub persist: bool,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 9090,
            token: String::new(),
            tls: TlsConfig::default(),
            persist: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CorsConfig {
    pub enabled: bool,
    pub allowed_origins: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub allowed_headers: Vec<String>,
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_origins: vec!["*".into()],
            allowed_methods: ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"]
                .map(String::from)
                .to_vec(),
            allowed_headers: vec!["authorization".into(), "content-type".into()],
            allow_credentials: false,
        }
    }
}

// ---------------------------------------------------------------- openapi

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenApiConfig {
    pub spec_path: String,
    pub validation: ValidationConfig,
    pub dynamic_responses: DynamicResponsesConfig,
}

impl Default for OpenApiConfig {
    fn default() -> Self {
        Self {
            spec_path: "openapi.yaml".into(),
            validation: ValidationConfig::default(),
            dynamic_responses: DynamicResponsesConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResponseValidation {
    Off,
    #[default]
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidationConfig {
    pub enabled: bool,
    /// What to do when the final response violates the spec: off | warn | error.
    pub responses: ResponseValidation,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            responses: ResponseValidation::Warn,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DynamicResponsesConfig {
    pub enabled: bool,
    pub array_items: usize,
}

impl Default for DynamicResponsesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            array_items: 1,
        }
    }
}

// ---------------------------------------------------------------- auth

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthType {
    #[default]
    None,
    /// Static API keys / bearer tokens (header or cookie), optionally several with roles.
    Token,
    Basic,
    /// Signed JWT (HS256/384/512, RS256/384/512, ES256/384) verified against a static key.
    Jwt,
    /// Reserved: introspection / JWKS discovery. Rejected at validation for now.
    Oauth2,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TokenSource {
    #[default]
    Header,
    Cookie,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiKey {
    pub value: String,
    pub subject: String,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenConfig {
    /// Single-token shorthand (subject "token", no roles).
    pub value: String,
    /// Several keys, each with a subject and roles.
    pub keys: Vec<ApiKey>,
    pub source: TokenSource,
    /// Header or cookie name. For `Authorization` a `Bearer ` prefix is accepted.
    pub name: String,
}

impl Default for TokenConfig {
    fn default() -> Self {
        Self {
            value: String::new(),
            keys: vec![],
            source: TokenSource::Header,
            name: "Authorization".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BasicConfig {
    pub username: String,
    pub password: String,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JwtConfig {
    /// HS256 | HS384 | HS512 | RS256 | RS384 | RS512 | ES256 | ES384
    pub algorithm: String,
    /// Shared secret for HS*.
    pub secret: String,
    /// PEM public key file for RS*/ES*.
    pub public_key_path: Option<String>,
    pub issuer: Option<String>,
    pub audience: Vec<String>,
    pub leeway_secs: u64,
    /// Claim holding roles (array of strings or space-separated string).
    pub roles_claim: String,
}

impl Default for JwtConfig {
    fn default() -> Self {
        Self {
            algorithm: "HS256".into(),
            secret: String::new(),
            public_key_path: None,
            issuer: None,
            audience: vec![],
            leeway_secs: 30,
            roles_claim: "roles".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub enabled: bool,
    pub auth_type: AuthType,
    pub token: TokenConfig,
    pub basic: BasicConfig,
    pub jwt: JwtConfig,
    /// Path prefixes that require authentication (`/admin` also protects `/admin/x`).
    pub protected_routes: Vec<String>,
    /// Also protect operations that declare `security` in the OpenAPI document.
    pub use_openapi_security: bool,
}

// ---------------------------------------------------------------- rate limit / security / logging

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RatePolicy {
    pub requests: u32,
    pub per_seconds: u64,
    /// Bucket size (default = `requests`).
    pub burst: Option<u32>,
    /// `ip` | `global` | `header:<name>`
    pub key: String,
}

impl Default for RatePolicy {
    fn default() -> Self {
        Self {
            requests: 100,
            per_seconds: 60,
            burst: None,
            key: "ip".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub enabled: bool,
    /// Applied to every route that has no policy of its own (bucket shared across routes).
    pub default: Option<RatePolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// nosniff, frame-deny, no-referrer, no-store on every response.
    pub headers: bool,
    /// Add `Strict-Transport-Security` (only when the data plane serves TLS).
    pub hsts: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            headers: true,
            hsts: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
    pub file: Option<String>,
    pub access_log: bool,
    /// Add an `x-stepwise-trace` response header with per-step timings.
    pub step_trace_header: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::Pretty,
            file: None,
            access_log: true,
            step_trace_header: false,
        }
    }
}

// ---------------------------------------------------------------- wasm hooks

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WasmConfig {
    /// Enable the WebAssembly hook runtime and the `/scripts` control API.
    pub enabled: bool,
    /// Artifact store + script index live here. Relative paths resolve against the config file.
    pub data_dir: String,
    /// Wall-clock budget for one hook call (including instantiation).
    pub hook_timeout_ms: u64,
    /// Linear-memory cap per hook call.
    pub max_memory_mb: usize,
    /// Hook calls running at once; extra calls wait (up to the timeout) then fail with 503.
    pub max_concurrent: usize,
    /// Cap on any JSON payload crossing the host/guest boundary, in either direction.
    pub max_payload_bytes: usize,
    /// Largest accepted component upload.
    pub max_upload_mb: usize,
    /// Provide WASI (clocks, random, stdio; no filesystem, env or network). Needed by Python and
    /// SpiderMonkey-based JS components; QuickJS components built with `--stub-wasi` need nothing.
    pub allow_wasi: bool,
    /// Pass `authorization` / `cookie` headers to hooks (off: they are removed).
    pub expose_credentials: bool,
    /// Optional builder service that compiles JS/TS/Python *source* into components.
    pub builder: BuilderConfig,
}

impl Default for WasmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            data_dir: "data".into(),
            hook_timeout_ms: 500,
            max_memory_mb: 128,
            max_concurrent: 32,
            max_payload_bytes: 1024 * 1024,
            max_upload_mb: 64,
            allow_wasi: true,
            expose_credentials: false,
            builder: BuilderConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BuilderConfig {
    /// `http://host:port` of a `stepwise-builder`. Empty disables source uploads.
    /// Plain HTTP only: keep the builder on a private network (or behind a TLS-terminating proxy).
    pub url: String,
    /// Must equal the builder's `BUILDER_TOKEN`. Use `${BUILDER_TOKEN}`.
    pub token: String,
    pub timeout_secs: u64,
}

impl Default for BuilderConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: String::new(),
            timeout_secs: 300,
        }
    }
}

// ---------------------------------------------------------------- database

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DbType {
    #[default]
    Sqlite,
    /// Reserved.
    Postgres,
    /// Reserved.
    Mysql,
    /// Reserved.
    Mongodb,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// When on, every operation that looks like a resource gets CRUD automatically
    /// (`GET /pets` lists, `POST /pets` creates, `GET /pets/{id}` reads, ...).
    pub enabled: bool,
    /// `sqlite` or `postgres`.
    pub db_type: DbType,
    /// SQLite file. Relative paths resolve against the config file.
    pub path: String,
    /// PostgreSQL connection string, e.g. `postgres://user:pass@host:5432/db?sslmode=require`.
    /// Use `${DATABASE_URL}`. With any `sslmode` other than `disable` the server certificate is
    /// always verified (against `tls_ca_path`, or the bundled public roots).
    pub url: String,
    /// PostgreSQL schema that holds Stepwise's tables (created if missing).
    pub schema: String,
    /// PEM bundle of CAs to trust for the PostgreSQL server certificate.
    pub tls_ca_path: Option<String>,
    /// Connections in the pool (both backends).
    pub pool_size: usize,
    pub busy_timeout_ms: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_type: DbType::Sqlite,
            path: "data/stepwise.db".into(),
            url: String::new(),
            schema: "public".into(),
            tls_ca_path: None,
            pool_size: 4,
            busy_timeout_ms: 5000,
        }
    }
}

pub use crate::db::crud::CrudConfig;

/// Collections the route's hooks may touch, beyond the route's own CRUD collection (`rw`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteDb {
    /// `{ inventory: r, audit: rw }`
    pub grants: BTreeMap<String, String>,
}

// ---------------------------------------------------------------- routes

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteConfig {
    /// Force auth on/off for this route, overriding protected_routes / OpenAPI security.
    pub auth: Option<bool>,
    /// Caller must have at least one of these roles (implies auth).
    pub require_roles: Vec<String>,
    pub rate_limit: Option<RatePolicy>,
    pub crud: CrudConfig,
    /// No-code data rules (owner scoping, soft delete, timestamps, defaults, templated effects, audit).
    pub data: crate::db::rules::DataRules,
    pub db: RouteDb,
    /// Which documented response status to serve (default: lowest 2xx).
    pub status: Option<u16>,
    pub steps: BTreeMap<Step, StepHooks>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StepHooks {
    pub before: Vec<String>,
    pub replace: Option<String>,
    pub after: Vec<String>,
}

// ---------------------------------------------------------------- loading

impl Config {
    pub fn from_yaml_str(s: &str) -> Result<Self> {
        let expanded = expand_env(s)?;
        let cfg: Config = serde_yaml::from_str(&expanded).context("invalid config YAML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load a config file; a relative `openapi.spec_path` is resolved against its directory.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let mut cfg = Self::from_yaml_str(&text)?;
        cfg.resolve_paths(path.parent().unwrap_or_else(|| Path::new(".")));
        Ok(cfg)
    }

    /// Make every file path in the config absolute relative to `dir`.
    pub fn resolve_paths(&mut self, dir: &Path) {
        let fix = |p: &mut String| {
            if !p.is_empty() && PathBuf::from(&*p).is_relative() {
                *p = dir.join(&*p).to_string_lossy().into_owned();
            }
        };
        fix(&mut self.openapi.spec_path);
        fix(&mut self.wasm.data_dir);
        fix(&mut self.database.path);
        if let Some(p) = self.database.tls_ca_path.as_mut() {
            fix(p);
        }
        for tls in [&mut self.server.tls, &mut self.control.tls] {
            fix(&mut tls.cert_path);
            fix(&mut tls.key_path);
            if let Some(p) = tls.client_ca_path.as_mut() {
                fix(p);
            }
        }
        if let Some(p) = self.auth.jwt.public_key_path.as_mut() {
            fix(p);
        }
        if let Some(p) = self.logging.file.as_mut() {
            fix(p);
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.control.enabled && self.control.token.is_empty() {
            bail!("control.token is required when the control plane is enabled (use `token: ${{STEPWISE_CONTROL_TOKEN}}`)");
        }
        for (name, tls) in [
            ("server.tls", &self.server.tls),
            ("control.tls", &self.control.tls),
        ] {
            if tls.enabled && (tls.cert_path.is_empty() || tls.key_path.is_empty()) {
                bail!("{name}.cert_path and {name}.key_path are required when TLS is enabled");
            }
        }
        if self.auth.enabled {
            match self.auth.auth_type {
                AuthType::Oauth2 => {
                    bail!("auth_type `oauth2` is not supported yet (use `jwt` with a static key)")
                }
                AuthType::Token => {
                    if self.auth.token.value.is_empty()
                        && self.auth.token.keys.iter().all(|k| k.value.is_empty())
                    {
                        bail!("auth_type `token` requires auth.token.value or auth.token.keys");
                    }
                }
                AuthType::Basic if self.auth.basic.username.is_empty() => {
                    bail!("auth_type `basic` requires auth.basic.username")
                }
                AuthType::Jwt => {
                    let j = &self.auth.jwt;
                    let hs = j.algorithm.starts_with("HS");
                    if hs && j.secret.is_empty() {
                        bail!("auth.jwt.secret is required for {}", j.algorithm);
                    }
                    if !hs && j.public_key_path.is_none() {
                        bail!("auth.jwt.public_key_path is required for {}", j.algorithm);
                    }
                }
                _ => {}
            }
        }
        let w = &self.wasm;
        if w.enabled
            && (w.hook_timeout_ms == 0
                || w.max_memory_mb == 0
                || w.max_concurrent == 0
                || w.max_payload_bytes == 0
                || w.max_upload_mb == 0)
        {
            bail!("wasm.hook_timeout_ms, max_memory_mb, max_concurrent, max_payload_bytes and max_upload_mb must be > 0");
        }
        crate::db::query::validate_relations(&self.relations).map_err(|e| anyhow::anyhow!(e))?;
        let d = &self.database;
        if d.enabled {
            if d.pool_size == 0 {
                bail!("database.pool_size must be > 0");
            }
            match d.db_type {
                DbType::Sqlite if d.path.is_empty() => {
                    bail!("database.path must be set for sqlite")
                }
                DbType::Sqlite => {}
                DbType::Postgres => {
                    if !(d.url.starts_with("postgres://") || d.url.starts_with("postgresql://")) {
                        bail!("database.url must be a postgres:// or postgresql:// connection string (use `${{DATABASE_URL}}`)");
                    }
                    if !crate::db::postgres::valid_schema(&d.schema) {
                        bail!("database.schema `{}` must be lower-case letters, digits and underscores", d.schema);
                    }
                }
                other => bail!(
                    "database.db_type `{other:?}` is not supported yet (sqlite and postgres are)"
                ),
            }
        }
        for (id, r) in &self.routes {
            if !r.data.is_empty() && !d.enabled {
                bail!("{id}: `data` rules need `database.enabled: true`");
            }
            r.data
                .validate()
                .map_err(|e| anyhow::anyhow!("{id}: {e}"))?;
            if !r.db.grants.is_empty() && !d.enabled {
                bail!("{id}: `db.grants` needs `database.enabled: true`");
            }
            for (coll, access) in &r.db.grants {
                if !crate::db::valid_collection(coll) {
                    bail!("{id}: db.grants collection `{coll}` must be 1-64 characters of [A-Za-z0-9_-]");
                }
                crate::db::Access::parse(access)
                    .with_context(|| format!("{id}: db.grants.{coll}"))?;
            }
        }
        let b = &self.wasm.builder;
        if !b.url.is_empty() {
            if !b.url.starts_with("http://") {
                bail!("wasm.builder.url must start with http:// (run the builder on a private network or behind a TLS-terminating proxy)");
            }
            if b.token.is_empty() || b.timeout_secs == 0 {
                bail!("wasm.builder.token and wasm.builder.timeout_secs must be set when wasm.builder.url is");
            }
        }
        let mut policies: Vec<(&str, &RatePolicy)> = vec![];
        if let Some(p) = &self.rate_limit.default {
            policies.push(("rate_limit.default", p));
        }
        for (id, r) in &self.routes {
            if let Some(p) = &r.rate_limit {
                policies.push((id.as_str(), p));
            }
        }
        for (name, p) in policies {
            if p.requests == 0 || p.per_seconds == 0 {
                bail!("{name}: rate limit `requests` and `per_seconds` must be > 0");
            }
            crate::ratelimit::RateKey::parse(&p.key)
                .with_context(|| format!("{name}: bad rate limit key"))?;
        }
        Ok(())
    }

    /// Copy with secrets replaced by `***` (what the control API returns).
    pub fn redacted(&self) -> Config {
        let mut c = self.clone();
        let hide = |s: &mut String| {
            if !s.is_empty() {
                *s = REDACTED.into();
            }
        };
        hide(&mut c.control.token);
        hide(&mut c.auth.token.value);
        for k in &mut c.auth.token.keys {
            hide(&mut k.value);
        }
        hide(&mut c.auth.basic.password);
        hide(&mut c.auth.jwt.secret);
        hide(&mut c.wasm.builder.token);
        c.database.url = redact_url_password(&c.database.url);
        c
    }

    /// Inverse of `redacted`: any `***` in `self` is replaced by the value in `current`.
    pub fn restore_secrets(&mut self, current: &Config) {
        let keep = |new: &mut String, old: &str| {
            if new == REDACTED {
                *new = old.to_string();
            }
        };
        keep(&mut self.control.token, &current.control.token);
        keep(&mut self.auth.token.value, &current.auth.token.value);
        keep(&mut self.auth.basic.password, &current.auth.basic.password);
        keep(&mut self.auth.jwt.secret, &current.auth.jwt.secret);
        keep(&mut self.wasm.builder.token, &current.wasm.builder.token);
        self.database.url = restore_url_password(&self.database.url, &current.database.url);
        for (i, k) in self.auth.token.keys.iter_mut().enumerate() {
            if let Some(old) = current.auth.token.keys.get(i) {
                keep(&mut k.value, &old.value);
            }
        }
    }
}

/// `postgres://user:secret@host/db` -> `postgres://user:***@host/db` (the rest stays visible).
pub fn redact_url_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    match userinfo.split_once(':') {
        Some((user, pw)) if !pw.is_empty() => format!("{scheme}://{user}:{REDACTED}@{host}{tail}"),
        _ => url.to_string(),
    }
}

/// Inverse of `redact_url_password`: a `***` password is replaced by the one in `current`.
pub fn restore_url_password(new: &str, current: &str) -> String {
    let marker = format!(":{REDACTED}@");
    let Some(i) = new.find(&marker) else {
        return new.to_string();
    };
    // the old password, taken from `current`'s userinfo
    let old_pw = current
        .split_once("://")
        .and_then(|(_, rest)| rest.rsplit_once('@').map(|(u, _)| u))
        .and_then(|u| u.split_once(':'))
        .map(|(_, pw)| pw)
        .unwrap_or_default();
    format!("{}:{}@{}", &new[..i], old_pw, &new[i + marker.len()..])
}

/// Expand `${NAME}` and `${NAME:-default}`. A missing variable with no default is an error.
pub fn expand_env(input: &str) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').context("unterminated ${...} in config")?;
        let expr = &after[..end];
        let (name, default) = match expr.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (expr, None),
        };
        match (std::env::var(name), default) {
            (Ok(v), _) => out.push_str(&v),
            (Err(_), Some(d)) => out.push_str(d),
            (Err(_), None) => {
                bail!("environment variable `{name}` is not set (referenced in config)")
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_expansion() {
        std::env::set_var("SW_TEST_A", "abc");
        assert_eq!(expand_env("x: ${SW_TEST_A}!").unwrap(), "x: abc!");
        assert_eq!(expand_env("x: ${SW_TEST_NOPE:-dflt}").unwrap(), "x: dflt");
        assert!(expand_env("x: ${SW_TEST_NOPE}").is_err());
        assert!(expand_env("x: ${oops").is_err());
    }

    #[test]
    fn redaction_roundtrip() {
        let c = Config::from_yaml_str("control:\n  token: sekret\nauth:\n  jwt:\n    secret: s3\n")
            .unwrap();
        let r = c.redacted();
        assert_eq!(r.control.token, REDACTED);
        let mut back = r.clone();
        back.restore_secrets(&c);
        assert_eq!(back.control.token, "sekret");
        assert_eq!(back.auth.jwt.secret, "s3");
    }

    #[test]
    fn database_url_password_is_redacted_and_restored() {
        let url = "postgres://app:s3cr%40t@db.internal:5432/stepwise?sslmode=require";
        let red = redact_url_password(url);
        assert_eq!(
            red,
            "postgres://app:***@db.internal:5432/stepwise?sslmode=require"
        );
        assert!(!red.contains("s3cr"));
        assert_eq!(restore_url_password(&red, url), url, "round trip");
        // changing the host while leaving *** keeps the stored password
        assert_eq!(
            restore_url_password("postgres://app:***@other:5432/x", url),
            "postgres://app:s3cr%40t@other:5432/x"
        );
        // a brand-new literal password is kept as typed; URLs without a password are untouched
        assert_eq!(
            restore_url_password("postgres://app:new@h/x", url),
            "postgres://app:new@h/x"
        );
        assert_eq!(
            redact_url_password("postgres://localhost/x"),
            "postgres://localhost/x"
        );
        assert_eq!(
            redact_url_password("postgres://app@localhost/x"),
            "postgres://app@localhost/x"
        );
    }

    #[test]
    fn control_token_is_mandatory() {
        assert!(Config::from_yaml_str("{}").is_err());
        assert!(Config::from_yaml_str("control: {enabled: false}").is_ok());
    }
}
