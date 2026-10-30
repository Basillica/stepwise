//! Shared application state and the transactional "apply a new config" machinery.
//!
//! Applying a config never half-applies: the candidate runtime is fully compiled (spec
//! parsed, schemas compiled, hooks bound) *before* anything is swapped or written.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::watch;

use crate::{
    compile::{build_runtime, Runtime},
    confdiff::{self, Action, Change},
    config::{Config, LoggingConfig, TlsConfig},
    db::DocStore,
    hooks::HookRegistry,
    metrics::Metrics,
    ratelimit::RateLimiter,
    wasm::{ScriptStore, WasmRuntime, WasmSupport},
};

pub type LogReloadFn = Box<dyn Fn(&LoggingConfig) -> Result<()> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListenerSpec {
    pub addr: SocketAddr,
    pub tls: TlsConfig,
}

impl ListenerSpec {
    pub fn mtls(&self) -> bool {
        self.tls.enabled && self.tls.client_ca_path.is_some()
    }
}

pub fn listener_specs(c: &Config) -> Result<(ListenerSpec, Option<ListenerSpec>)> {
    let addr = |host: &str, port: u16, what: &str| -> Result<SocketAddr> {
        let ip: IpAddr = host
            .parse()
            .with_context(|| format!("{what}.host must be an IP address, got `{host}`"))?;
        Ok(SocketAddr::new(ip, port))
    };
    let data = ListenerSpec {
        addr: addr(&c.server.host, c.server.port, "server")?,
        tls: c.server.tls.clone(),
    };
    let control = if c.control.enabled {
        Some(ListenerSpec {
            addr: addr(&c.control.host, c.control.port, "control")?,
            tls: c.control.tls.clone(),
        })
    } else {
        None
    };
    Ok((data, control))
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub applied: bool,
    pub dry_run: bool,
    pub version: u64,
    pub changes: Vec<Change>,
    /// Listeners that will be (or would be) gracefully restarted.
    pub restarts: Vec<&'static str>,
    /// Paths saved but only effective after a process restart.
    pub pending_process_restart: Vec<String>,
    pub persisted: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("invalid configuration: {0:#}")]
    Invalid(anyhow::Error),
    #[error("config was modified by someone else (current version is {current})")]
    Conflict { current: u64 },
    #[error("could not persist config file: {0:#}")]
    Persist(anyhow::Error),
}

#[derive(Debug, Clone, Default)]
pub struct ApplyOpts {
    pub dry_run: bool,
    pub if_match: Option<u64>,
}

pub struct AppState {
    pub runtime: ArcSwap<Runtime>,
    pub registry: Arc<HookRegistry>,
    pub limiter: Arc<RateLimiter>,
    pub metrics: Arc<Metrics>,
    pub db: Option<Arc<dyn DocStore>>,
    pub scripts: Option<Arc<ScriptStore>>,
    pub wasm: Option<Arc<WasmRuntime>>,
    pub started: Instant,
    pub config_path: Option<PathBuf>,
    pub data_listener: watch::Sender<Option<ListenerSpec>>,
    pub control_listener: watch::Sender<Option<ListenerSpec>>,
    version: AtomicU64,
    apply_lock: tokio::sync::Mutex<()>,
    raw_yaml: Mutex<serde_yaml::Value>,
    pending_restart: Mutex<BTreeSet<String>>,
    bound: Mutex<BTreeMap<&'static str, SocketAddr>>,
    log_reload: Option<LogReloadFn>,
}

impl AppState {
    pub fn new(
        config: Config,
        config_path: Option<PathBuf>,
        registry: HookRegistry,
        log_reload: Option<LogReloadFn>,
    ) -> Result<Arc<Self>> {
        let mut registry = registry;
        let limiter = Arc::new(RateLimiter::new());
        let metrics = Arc::new(Metrics::default());
        let (scripts, wasm) = if config.wasm.enabled {
            let store = ScriptStore::open(Path::new(&config.wasm.data_dir))
                .with_context(|| format!("opening wasm data_dir `{}`", config.wasm.data_dir))?;
            let rt = WasmRuntime::new(&config.wasm, store.clone(), metrics.clone())?;
            registry.attach_wasm(WasmSupport {
                runtime: rt.clone(),
                store: store.clone(),
            });
            (Some(store), Some(rt))
        } else {
            (None, None)
        };
        let db: Option<Arc<dyn DocStore>> = if config.database.enabled {
            Some(crate::db::open(&config.database).context("opening the database")?)
        } else {
            None
        };
        let registry = Arc::new(registry);
        let config = Arc::new(config);
        let runtime = build_runtime(
            config.clone(),
            &registry,
            limiter.clone(),
            metrics.clone(),
            db.clone(),
        )?;
        let (data, control) = listener_specs(&config)?;
        let raw_yaml = config_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_yaml::from_str(&t).ok())
            .unwrap_or(serde_yaml::Value::Mapping(Default::default()));
        Ok(Arc::new(Self {
            runtime: ArcSwap::from_pointee(runtime),
            registry,
            limiter,
            metrics,
            db,
            scripts,
            wasm,
            started: Instant::now(),
            config_path,
            data_listener: watch::channel(Some(data)).0,
            control_listener: watch::channel(control).0,
            version: AtomicU64::new(1),
            apply_lock: tokio::sync::Mutex::new(()),
            raw_yaml: Mutex::new(raw_yaml),
            pending_restart: Mutex::new(BTreeSet::new()),
            bound: Mutex::new(BTreeMap::new()),
            log_reload,
        }))
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    pub fn config(&self) -> Arc<Config> {
        self.runtime.load().config.clone()
    }

    pub fn pending_process_restart(&self) -> Vec<String> {
        self.pending_restart
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn set_bound(&self, name: &'static str, addr: SocketAddr) {
        self.bound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name, addr);
    }

    pub fn bound(&self, name: &str) -> Option<SocketAddr> {
        self.bound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .copied()
    }

    /// Apply a full config document supplied by a client (JSON, secrets may be `***`).
    pub async fn apply_json(
        &self,
        incoming: Value,
        opts: ApplyOpts,
    ) -> Result<ApplyReport, ApplyError> {
        let mut new: Config = serde_json::from_value(incoming.clone())
            .map_err(|e| ApplyError::Invalid(anyhow::anyhow!("{e}")))?;
        let cur = self.config();
        new.restore_secrets(&cur);
        new.resolve_paths(self.config_dir().as_path());
        // What the client saw vs what it sent: that's the set of edits to persist.
        let seen = confdiff::to_json(&cur.redacted());
        let edits = confdiff::leaf_diff(&seen, &incoming);
        self.apply_inner(new, Some((edits, incoming)), opts, false)
            .await
    }

    /// Re-read the config file and OpenAPI spec from disk and apply them.
    pub async fn reload_from_disk(&self, opts: ApplyOpts) -> Result<ApplyReport, ApplyError> {
        let path = self.config_path.clone().ok_or_else(|| {
            ApplyError::Invalid(anyhow::anyhow!("server was started without a config file"))
        })?;
        let new = Config::load(&path).map_err(ApplyError::Invalid)?;
        if !opts.dry_run {
            if let Some(raw) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_yaml::from_str(&t).ok())
            {
                *self.raw_yaml.lock().unwrap_or_else(|e| e.into_inner()) = raw;
            }
        }
        self.apply_inner(new, None, opts, true).await
    }

    /// Rebuild the runtime from the *current* config (e.g. after a script alias moved) so that
    /// `@alias` references re-resolve. Same all-or-nothing guarantees as any other apply.
    pub async fn rebuild_runtime(&self) -> Result<ApplyReport, ApplyError> {
        let cfg = (*self.config()).clone();
        self.apply_inner(cfg, None, ApplyOpts::default(), true)
            .await
    }

    fn config_dir(&self) -> PathBuf {
        self.config_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    async fn apply_inner(
        &self,
        new: Config,
        persist: Option<(Vec<Vec<String>>, Value)>,
        opts: ApplyOpts,
        force_bump: bool,
    ) -> Result<ApplyReport, ApplyError> {
        let _guard = self.apply_lock.lock().await;
        let current_version = self.version();
        if let Some(v) = opts.if_match {
            if v != current_version {
                self.metrics.inc_config_apply("conflict");
                return Err(ApplyError::Conflict {
                    current: current_version,
                });
            }
        }
        let cur = self.config();
        let new = Arc::new(new);

        // 1. Prove the candidate is good *before* touching anything.
        let candidate = {
            let (cfg, reg, lim, met, db) = (
                new.clone(),
                self.registry.clone(),
                self.limiter.clone(),
                self.metrics.clone(),
                self.db.clone(),
            );
            tokio::task::spawn_blocking(move || build_runtime(cfg, &reg, lim, met, db))
                .await
                .map_err(|e| ApplyError::Invalid(anyhow::anyhow!("build task failed: {e}")))?
        }
        .map_err(|e| {
            self.metrics.inc_config_apply("rejected");
            ApplyError::Invalid(e)
        })?;
        let (data_spec, control_spec) = listener_specs(&new).map_err(|e| {
            self.metrics.inc_config_apply("rejected");
            ApplyError::Invalid(e)
        })?;

        let changes = confdiff::diff(&cur, &new);
        let mut restarts = Vec::new();
        if changes
            .iter()
            .any(|c| c.action == Action::RestartDataListener)
        {
            restarts.push("data_plane");
        }
        if changes
            .iter()
            .any(|c| c.action == Action::RestartControlListener)
        {
            restarts.push("control_plane");
        }
        let process_paths: Vec<String> = changes
            .iter()
            .filter(|c| c.action == Action::RestartProcess)
            .map(|c| c.path.clone())
            .collect();
        let mut warnings = Vec::new();
        for c in &changes {
            if is_secret_path(&c.path) {
                warnings.push(format!("`{}` changed; a literal secret in the config file is risky, prefer ${{ENV_VAR}}", c.path));
            }
        }
        let mut report = ApplyReport {
            applied: false,
            dry_run: opts.dry_run,
            version: current_version,
            changes,
            restarts,
            pending_process_restart: process_paths.clone(),
            persisted: false,
            warnings,
        };
        if opts.dry_run {
            return Ok(report);
        }

        // 2. Persist (file first: if the disk write fails, nothing has changed).
        if let (Some((edits, incoming)), Some(path)) = (persist, self.config_path.as_ref()) {
            if new.control.persist && !edits.is_empty() {
                let mut raw = self.raw_yaml.lock().unwrap_or_else(|e| e.into_inner());
                let mut patched = raw.clone();
                confdiff::patch_raw(&mut patched, &edits, &incoming);
                write_atomic(
                    path,
                    &serde_yaml::to_string(&patched).map_err(|e| ApplyError::Persist(e.into()))?,
                )
                .map_err(ApplyError::Persist)?;
                *raw = patched;
                report.persisted = true;
            }
        }

        // 3. Swap, then nudge listeners / logging.
        self.runtime.store(Arc::new(candidate));
        let bump = force_bump || !report.changes.is_empty();
        if bump {
            report.version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        }
        self.data_listener.send_if_modified(|cur| {
            let new = Some(data_spec.clone());
            (*cur != new).then(|| *cur = new).is_some()
        });
        self.control_listener.send_if_modified(|cur| {
            (*cur != control_spec)
                .then(|| *cur = control_spec.clone())
                .is_some()
        });
        if report
            .changes
            .iter()
            .any(|c| c.path.starts_with("logging.level"))
        {
            if let Some(f) = &self.log_reload {
                if let Err(e) = f(&new.logging) {
                    report
                        .warnings
                        .push(format!("log level not applied: {e:#}"));
                }
            }
        }
        if !process_paths.is_empty() {
            self.pending_restart
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(process_paths);
        }
        report.applied = true;
        self.metrics.inc_config_apply("ok");
        tracing::info!(version = report.version, changes = report.changes.len(), restarts = ?report.restarts, "config applied");
        Ok(report)
    }
}

fn is_secret_path(p: &str) -> bool {
    matches!(
        p,
        "control.token"
            | "auth.token.value"
            | "auth.basic.password"
            | "auth.jwt.secret"
            | "wasm.builder.token"
            | "database.url"
    ) || p.starts_with("auth.token.keys")
}

/// Write via temp file + rename so readers never see a half-written config; keep a `.bak`.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    if path.exists() {
        let _ = std::fs::copy(path, path.with_extension("yaml.bak"));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
