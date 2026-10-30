//! Wasmtime engine, module cache and the (sandboxed) call path.
//!
//! Speed comes from doing the expensive work *once, off the request path*:
//!   * upload  → validate + compile → serialized `.cwasm` next to the artifact
//!   * build   → `Component::deserialize_file` (mmap, no recompilation) + `instantiate_pre`
//!   * request → one pooled instantiation from the pre-linked module, a fresh `Store`
//!
//! Isolation: every call gets a new `Store` (no state shared between requests), a memory cap,
//! cooperative epoch yielding (a spinning guest cannot starve the server) and a wall-clock
//! timeout that drops the guest mid-execution.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio::sync::Semaphore;
use wasmtime::{
    component::{Component, HasSelf, Linker, ResourceTable},
    Config, Engine, InstanceAllocationStrategy, OptLevel, PoolingAllocationConfig, Store,
    StoreLimits, StoreLimitsBuilder,
};
use wasmtime_wasi::{p2::pipe::MemoryOutputPipe, WasiCtx, WasiCtxView, WasiView};

use super::{
    bindings::{self, stepwise::hook::host::Level},
    store::ScriptStore,
};
use crate::{
    config::WasmConfig,
    db::{valid_collection, DbAccess, Effect, ListQuery},
    metrics::Metrics,
};

pub use bindings::stepwise::hook::types::{Context as WitContext, Outcome as WitOutcome};

/// `wasmtime::Error` is its own type in Wasmtime 43; bridge it into `anyhow` (keeping the cause chain).
fn w<T>(r: wasmtime::Result<T>) -> Result<T> {
    r.map_err(|e| anyhow::anyhow!("{e:#}"))
}

const EPOCH_TICK: Duration = Duration::from_millis(10);
const MAX_LOG_LINES: usize = 200;
const MAX_LOG_LINE_CHARS: usize = 4096;
const MAX_STDIO_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, serde::Serialize)]
pub struct LogLine {
    /// debug | info | warn | error | stdout | stderr
    pub level: &'static str,
    pub message: String,
}

/// Per-call state living inside the guest's `Store`.
pub struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    logs: Vec<LogLine>,
    meta: CallMeta,
    db: Option<DbAccess>,
    effects: Vec<Effect>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl bindings::stepwise::hook::types::Host for HostState {}

impl HostState {
    fn db(&self, collection: &str, write: bool) -> Result<&DbAccess, String> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| "the database is not available to this hook".to_string())?;
        if !valid_collection(collection) {
            return Err(format!("invalid collection name `{collection}`"));
        }
        db.check(collection, write)?;
        Ok(db)
    }
}

/// Store errors are logged in full but only the safe part reaches the guest.
fn guest_err(what: &str, e: anyhow::Error) -> String {
    let msg = e.to_string();
    tracing::warn!(target: "stepwise::hook", error = %format!("{e:#}"), "database call failed ({what})");
    if msg.starts_with("this run is read-only") {
        msg
    } else {
        format!("database error during {what}")
    }
}

fn doc(db: &DbAccess, what: &str, text: &str) -> Result<serde_json::Value, String> {
    if text.len() > db.max_payload {
        return Err(format!("{what} is over wasm.max_payload_bytes"));
    }
    serde_json::from_str(text).map_err(|e| format!("{what} is not valid JSON: {e}"))
}

/// Most effects one hook call may queue.
const MAX_EFFECTS_PER_CALL: usize = 100;

impl bindings::stepwise::hook::effects::Host for HostState {
    async fn emit(&mut self, effect: String) -> Result<(), String> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| "the database is not available to this hook".to_string())?;
        if effect.len() > db.max_payload {
            return Err("effect is over wasm.max_payload_bytes".into());
        }
        if self.effects.len() >= MAX_EFFECTS_PER_CALL {
            return Err(format!(
                "a hook may queue at most {MAX_EFFECTS_PER_CALL} effects per call"
            ));
        }
        let e: Effect =
            serde_json::from_str(&effect).map_err(|e| format!("not a valid effect: {e}"))?;
        e.validate()?;
        db.check(&e.collection, true)?;
        self.effects.push(e);
        Ok(())
    }
}

impl bindings::stepwise::hook::query::Host for HostState {
    async fn run(&mut self, query: String) -> Result<String, String> {
        use crate::db::query::{parse_query, Engine, Limits, QueryError};
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| "the database is not available to this hook".to_string())?;
        if query.len() > db.max_payload {
            return Err("query is over wasm.max_payload_bytes".into());
        }
        let v: serde_json::Value =
            serde_json::from_str(&query).map_err(|e| format!("query is not valid JSON: {e}"))?;
        let limits = Limits::default();
        let q = parse_query(&v, &db.relations, limits.max_depth)?;
        let access = db.clone();
        let can_read = move |c: &str| access.check(c, false);
        let out = Engine { store: db.store.as_ref(), limits, can_read: &can_read }.run(&q).await.map_err(|e| match e {
            QueryError::Backend(inner) => {
                tracing::warn!(target: "stepwise::hook", error = %format!("{inner:#}"), "database call failed (query)");
                "database error during query".to_string()
            }
            other => other.to_string(),
        })?;
        let text = out.to_string();
        if text.len() > db.max_payload {
            return Err(format!("the result is {} bytes, over wasm.max_payload_bytes; narrow the query (where/limit/select)", text.len()));
        }
        Ok(text)
    }
}

impl bindings::stepwise::hook::db::Host for HostState {
    async fn get(&mut self, collection: String, id: String) -> Result<Option<String>, String> {
        let db = self.db(&collection, false)?;
        db.store
            .get(&collection, &id)
            .await
            .map(|d| d.map(|v| v.to_string()))
            .map_err(|e| guest_err("get", e))
    }

    async fn find(
        &mut self,
        collection: String,
        filters: Vec<(String, String)>,
        limit: u32,
    ) -> Result<Vec<String>, String> {
        let db = self.db(&collection, false)?;
        let q = ListQuery {
            filters,
            limit: (limit as usize).clamp(1, 1000),
            ..Default::default()
        };
        db.store
            .list(&collection, &q)
            .await
            .map(|v| v.iter().map(|d| d.to_string()).collect())
            .map_err(|e| guest_err("list", e))
    }

    async fn count(
        &mut self,
        collection: String,
        filters: Vec<(String, String)>,
    ) -> Result<u64, String> {
        let db = self.db(&collection, false)?;
        db.store
            .count(&collection, &filters)
            .await
            .map_err(|e| guest_err("count", e))
    }

    async fn insert(&mut self, collection: String, id: String, d: String) -> Result<bool, String> {
        let db = self.db(&collection, true)?;
        let value = doc(db, "doc", &d)?;
        db.store
            .insert(&collection, &id, &value)
            .await
            .map_err(|e| guest_err("insert", e))
    }

    async fn replace(&mut self, collection: String, id: String, d: String) -> Result<bool, String> {
        let db = self.db(&collection, true)?;
        let value = doc(db, "doc", &d)?;
        db.store
            .replace(&collection, &id, &value)
            .await
            .map_err(|e| guest_err("replace", e))
    }

    async fn merge(
        &mut self,
        collection: String,
        id: String,
        patch: String,
    ) -> Result<Option<String>, String> {
        let db = self.db(&collection, true)?;
        let value = doc(db, "patch", &patch)?;
        db.store
            .merge(&collection, &id, &value)
            .await
            .map(|d| d.map(|v| v.to_string()))
            .map_err(|e| guest_err("merge", e))
    }

    async fn delete(&mut self, collection: String, id: String) -> Result<bool, String> {
        let db = self.db(&collection, true)?;
        db.store
            .delete(&collection, &id)
            .await
            .map_err(|e| guest_err("delete", e))
    }
}

impl bindings::stepwise::hook::host::Host for HostState {
    async fn log(&mut self, level: Level, message: String) {
        let (name, m) = (&self.meta.hook, &message);
        match level {
            Level::Debug => {
                tracing::debug!(target: "stepwise::hook", hook = %name, route = %self.meta.route, request_id = %self.meta.request_id, "{m}")
            }
            Level::Info => {
                tracing::info!(target: "stepwise::hook", hook = %name, route = %self.meta.route, request_id = %self.meta.request_id, "{m}")
            }
            Level::Warn => {
                tracing::warn!(target: "stepwise::hook", hook = %name, route = %self.meta.route, request_id = %self.meta.request_id, "{m}")
            }
            Level::Error => {
                tracing::error!(target: "stepwise::hook", hook = %name, route = %self.meta.route, request_id = %self.meta.request_id, "{m}")
            }
        }
        if self.logs.len() < MAX_LOG_LINES {
            let lvl = match level {
                Level::Debug => "debug",
                Level::Info => "info",
                Level::Warn => "warn",
                Level::Error => "error",
            };
            self.logs.push(LogLine {
                level: lvl,
                message: message.chars().take(MAX_LOG_LINE_CHARS).collect(),
            });
        }
    }
}

#[derive(Clone)]
pub struct CallMeta {
    pub hook: Arc<str>,
    pub route: Arc<str>,
    pub request_id: Arc<str>,
}

/// A component that passed validation, linked and ready to instantiate cheaply.
pub struct LoadedModule {
    pub digest: String,
    pre: bindings::HookPre<HostState>,
}

#[derive(Debug)]
pub enum CallFailure {
    Timeout,
    Busy,
    Trap(String),
}

pub struct CallResult {
    pub outcome: Result<WitOutcome, CallFailure>,
    /// Effects the guest queued (only meaningful when the call succeeded).
    pub effects: Vec<Effect>,
    pub logs: Vec<LogLine>,
    pub elapsed: Duration,
}

pub struct WasmRuntime {
    engine: Engine,
    linker: Linker<HostState>,
    store: Arc<ScriptStore>,
    modules: RwLock<HashMap<String, Arc<LoadedModule>>>,
    permits: Arc<Semaphore>,
    max_memory: usize,
    pub metrics: Arc<Metrics>,
    stop: Arc<AtomicBool>,
}

impl Drop for WasmRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl WasmRuntime {
    pub fn new(
        cfg: &WasmConfig,
        store: Arc<ScriptStore>,
        metrics: Arc<Metrics>,
    ) -> Result<Arc<Self>> {
        let max_memory = cfg.max_memory_mb * 1024 * 1024;
        let engine = match Engine::new(&engine_config(cfg, true)) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "pooling allocator unavailable, using on-demand allocation");
                w(Engine::new(&engine_config(cfg, false)))
                    .context("creating the WebAssembly engine")?
            }
        };

        let mut linker: Linker<HostState> = Linker::new(&engine);
        if cfg.allow_wasi {
            w(wasmtime_wasi::p2::add_to_linker_async(&mut linker)).context("linking WASI")?;
        }
        w(bindings::Hook::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s))
            .context("linking the stepwise host interface")?;

        // Epoch clock: drives cooperative yielding. Stops when the runtime is dropped.
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (engine, stop) = (engine.clone(), stop.clone());
            std::thread::Builder::new()
                .name("stepwise-epoch".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(EPOCH_TICK);
                        engine.increment_epoch();
                    }
                })
                .context("starting epoch thread")?;
        }

        Ok(Arc::new(Self {
            engine,
            linker,
            store,
            modules: RwLock::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(cfg.max_concurrent)),
            max_memory,
            metrics,
            stop,
        }))
    }

    /// Validate an uploaded component against the host contract and write its precompiled form.
    /// CPU-heavy (seconds for Python); call from `spawn_blocking`.
    pub fn compile_and_verify(&self, digest: &str, bytes: &[u8]) -> Result<()> {
        let component =
            w(Component::new(&self.engine, bytes)).context("not a valid WebAssembly component")?;
        let pre = w(self.linker.instantiate_pre(&component))
            .context("the component imports something this host does not provide (check allow_wasi and the WIT world)")?;
        w(bindings::HookPre::new(pre))
            .context("the component does not export `stepwise:hook/handler` (was it built against wit/stepwise-hook.wit?)")?;
        let ser = w(component.serialize()).context("serializing compiled component")?;
        let path = self.store.cwasm_path(digest)?;
        let tmp = path.with_extension("cwasm.tmp");
        std::fs::write(&tmp, ser)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Load (and cache) a module by digest. Uses the precompiled form when it is still valid
    /// for this engine, otherwise recompiles. Blocking; runs at config-build time.
    pub fn load(&self, digest: &str) -> Result<Arc<LoadedModule>> {
        if let Some(m) = self
            .modules
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(digest)
        {
            return Ok(m.clone());
        }
        let cwasm = self.store.cwasm_path(digest)?;
        let from_cache = if cwasm.exists() {
            // SAFETY: the file was produced by `Component::serialize` from our own engine config and
            // lives in our private data_dir; Wasmtime also rejects incompatible engine versions.
            unsafe { Component::deserialize_file(&self.engine, &cwasm) }.ok()
        } else {
            None
        };
        let component = match from_cache {
            Some(c) => c,
            None => {
                let bytes = self.store.read_artifact(digest)?;
                self.compile_and_verify(digest, &bytes)?;
                // SAFETY: as above, we just wrote this file.
                w(unsafe { Component::deserialize_file(&self.engine, &cwasm) })
                    .context("loading freshly compiled component")?
            }
        };
        let pre = w(self.linker.instantiate_pre(&component)).context("linking component")?;
        let module = Arc::new(LoadedModule {
            digest: digest.to_string(),
            pre: w(bindings::HookPre::new(pre))?,
        });
        self.modules
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(digest.to_string(), module.clone());
        Ok(module)
    }

    /// Drop a cached module (after its artifact is deleted).
    pub fn evict(&self, digest: &str) {
        self.modules
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(digest);
    }

    pub async fn invoke(
        &self,
        m: &LoadedModule,
        input: &WitContext,
        timeout: Duration,
        meta: CallMeta,
        db: Option<DbAccess>,
    ) -> CallResult {
        let t0 = Instant::now();
        let permit = match tokio::time::timeout(timeout, self.permits.clone().acquire_owned()).await
        {
            Ok(Ok(p)) => p,
            _ => {
                return CallResult {
                    outcome: Err(CallFailure::Busy),
                    effects: vec![],
                    logs: vec![],
                    elapsed: t0.elapsed(),
                }
            }
        };

        let stdout = MemoryOutputPipe::new(MAX_STDIO_BYTES);
        let stderr = MemoryOutputPipe::new(MAX_STDIO_BYTES);
        let mut wasi = WasiCtx::builder();
        wasi.stdout(stdout.clone()).stderr(stderr.clone());
        wasi.allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false);
        wasi.socket_addr_check(|_, _| Box::pin(async { false }));
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.max_memory)
            .table_elements(100_000)
            .instances(32)
            .memories(8)
            .tables(8)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                wasi: wasi.build(),
                table: ResourceTable::new(),
                limits,
                logs: Vec::new(),
                meta,
                db,
                effects: Vec::new(),
            },
        );
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(1);
        store.epoch_deadline_async_yield_and_update(1);

        let budget = timeout.saturating_sub(t0.elapsed());
        let res = tokio::time::timeout(budget, async {
            let inst = m.pre.instantiate_async(&mut store).await?;
            inst.stepwise_hook_handler()
                .call_handle(&mut store, input)
                .await
        })
        .await;

        let effects = std::mem::take(&mut store.data_mut().effects);
        let mut logs = std::mem::take(&mut store.data_mut().logs);
        for (level, pipe) in [("stdout", &stdout), ("stderr", &stderr)] {
            for line in String::from_utf8_lossy(&pipe.contents()).lines().take(50) {
                logs.push(LogLine {
                    level,
                    message: line.chars().take(MAX_LOG_LINE_CHARS).collect(),
                });
            }
        }
        drop(store);
        drop(permit);
        let outcome = match res {
            Ok(Ok(o)) => Ok(o),
            Ok(Err(e)) => Err(CallFailure::Trap(format!("{e:#}"))),
            Err(_) => Err(CallFailure::Timeout),
        };
        CallResult {
            outcome,
            effects,
            logs,
            elapsed: t0.elapsed(),
        }
    }
}

fn engine_config(cfg: &WasmConfig, pooling: bool) -> Config {
    let mut c = Config::new();
    c.wasm_component_model(true);
    c.epoch_interruption(true);
    c.cranelift_opt_level(OptLevel::Speed);
    c.parallel_compilation(true);
    if pooling {
        let n = cfg.max_concurrent as u32;
        let mut pool = PoolingAllocationConfig::new();
        pool.total_component_instances(n)
            .max_core_instances_per_component(32)
            .total_core_instances(n * 32)
            .max_memories_per_component(4)
            .max_memories_per_module(2)
            .total_memories(n * 4)
            .max_tables_per_component(4)
            .max_tables_per_module(2)
            .total_tables(n * 4)
            .total_stacks(n)
            .max_memory_size(cfg.max_memory_mb * 1024 * 1024)
            .table_elements(100_000);
        c.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    }
    c
}
