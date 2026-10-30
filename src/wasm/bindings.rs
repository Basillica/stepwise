//! Typed host/guest bindings generated from `wit/stepwise-hook.wit`.

wasmtime::component::bindgen!({
    path: "wit",
    world: "hook",
    imports: { default: async },
    exports: { default: async },
});
