//! The hook seam. A `Hook` is anything that can act on the request context at a
//! step. Today: in-process Rust hooks. Next: a `WasmHook` that implements this same
//! trait by serializing the context across the component boundary.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use serde_json::json;

use crate::{
    error::ApiError,
    pipeline::{Outcome, RequestContext, Step},
};

#[async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    /// Immutable identity of what this hook runs (e.g. an artifact digest), if it has one.
    fn pinned(&self) -> Option<String> {
        None
    }
    async fn call(&self, step: Step, ctx: &mut RequestContext) -> Outcome;
}

#[derive(Default)]
pub struct HookRegistry {
    hooks: HashMap<String, Arc<dyn Hook>>,
    wasm: Option<crate::wasm::WasmSupport>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(Stamp));
        r.register(Arc::new(Deny));
        r.register(Arc::new(Echo));
        r
    }

    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        self.hooks.insert(hook.name().to_string(), hook);
    }

    pub fn attach_wasm(&mut self, wasm: crate::wasm::WasmSupport) {
        self.wasm = Some(wasm);
    }

    pub fn wasm(&self) -> Option<&crate::wasm::WasmSupport> {
        self.wasm.as_ref()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Hook>> {
        self.hooks.get(name).cloned()
    }

    /// Turn a hook reference from the config into a bound hook. `wasm:name@ref` references are
    /// resolved to an artifact digest and loaded here, at build time, never on the request path.
    pub fn resolve(
        &self,
        name: &str,
        wasm_cfg: &crate::config::WasmConfig,
    ) -> anyhow::Result<Arc<dyn Hook>> {
        if name.starts_with("wasm:") {
            let w = self.wasm.as_ref().ok_or_else(|| {
                anyhow::anyhow!("hook `{name}` needs the WASM runtime; set `wasm.enabled: true`")
            })?;
            return w.bind(name, wasm_cfg);
        }
        self.get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown hook `{name}` (builtin hooks: {:?}; scripts are `wasm:<name>@<version|alias|sha256:…>`)", self.names()))
    }

    pub fn names(&self) -> Vec<String> {
        let mut n: Vec<_> = self.hooks.keys().cloned().collect();
        n.sort();
        n
    }
}

/// Adds `x-stepwise-stamp: <step>` to the response. Handy to see where a hook ran.
struct Stamp;

#[async_trait]
impl Hook for Stamp {
    fn name(&self) -> &str {
        "builtin:stamp"
    }
    async fn call(&self, step: Step, ctx: &mut RequestContext) -> Outcome {
        ctx.response
            .headers
            .push(("x-stepwise-stamp".into(), step.as_str().into()));
        Outcome::Continue
    }
}

/// Rejects the request with 403. Useful on `authorize` to test that gating works.
struct Deny;

#[async_trait]
impl Hook for Deny {
    fn name(&self) -> &str {
        "builtin:deny"
    }
    async fn call(&self, _step: Step, _ctx: &mut RequestContext) -> Outcome {
        Outcome::Fail(ApiError::new(403, "forbidden", "denied by builtin:deny"))
    }
}

/// Replacement for `execute`: echoes what the server understood about the request.
struct Echo;

#[async_trait]
impl Hook for Echo {
    fn name(&self) -> &str {
        "builtin:echo"
    }
    async fn call(&self, _step: Step, ctx: &mut RequestContext) -> Outcome {
        ctx.response.status = 200;
        ctx.response.body = Some(json!({
            "path_params": ctx.path_params,
            "query": ctx.query,
            "body": ctx.body,
            "identity": ctx.identity,
        }));
        Outcome::Continue
    }
}
