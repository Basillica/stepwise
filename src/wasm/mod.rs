//! WebAssembly hooks: polyglot business logic (Rust, JS/TS, Python, anything that targets the
//! component model) bound to pipeline steps through the `stepwise:hook` WIT world.

pub mod bindings;
pub mod builder;
pub mod hook;
pub mod runtime;
pub mod store;

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};

use crate::{config::WasmConfig, hooks::Hook};
use hook::WasmHook;
pub use runtime::WasmRuntime;
pub use store::ScriptStore;

/// What the hook registry needs to resolve `wasm:` references.
#[derive(Clone)]
pub struct WasmSupport {
    pub runtime: Arc<WasmRuntime>,
    pub store: Arc<ScriptStore>,
}

impl WasmSupport {
    pub fn bind_concrete(&self, reference: &str, cfg: &WasmConfig) -> Result<Arc<WasmHook>> {
        let (name, r) = store::parse_ref(reference)
            .ok_or_else(|| anyhow!("`{reference}` is not a wasm reference"))?;
        let resolved = self.store.resolve(name, r)?;
        let module = self.runtime.load(&resolved.digest)?;
        Ok(Arc::new(WasmHook {
            reference: reference.to_string(),
            resolved,
            module,
            runtime: self.runtime.clone(),
            timeout: Duration::from_millis(cfg.hook_timeout_ms),
            max_payload: cfg.max_payload_bytes,
            expose_credentials: cfg.expose_credentials,
        }))
    }

    pub fn bind(&self, reference: &str, cfg: &WasmConfig) -> Result<Arc<dyn Hook>> {
        Ok(self.bind_concrete(reference, cfg)?)
    }
}
