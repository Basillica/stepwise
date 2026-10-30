//! Runs one listener and keeps it matching the desired `ListenerSpec`. When the spec
//! changes, the old listener is drained gracefully and a new one is bound. If the new
//! one cannot bind, the previous good spec is restored so a bad edit never causes an outage.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::Router;
use axum_server::{tls_rustls::RustlsConfig, Handle};
use tokio::sync::watch;

use crate::{
    state::{AppState, ListenerSpec},
    tls,
};

const DRAIN: Duration = Duration::from_secs(10);

pub async fn run_listener(
    name: &'static str,
    state: Arc<AppState>,
    mut spec_rx: watch::Receiver<Option<ListenerSpec>>,
    make_router: fn(Arc<AppState>) -> Router,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut last_good: Option<ListenerSpec> = None;
    let mut force: Option<ListenerSpec> = None;

    loop {
        if *shutdown.borrow() {
            return;
        }
        let wanted = match force.take() {
            Some(s) => Some(s),
            None => spec_rx.borrow_and_update().clone(),
        };
        let Some(spec) = wanted else {
            tokio::select! {
                r = spec_rx.changed() => { if r.is_err() { return; } continue; }
                _ = shutdown.changed() => return,
            }
        };

        let tls_cfg = if spec.tls.enabled {
            match tls::build_server_config(&spec.tls) {
                Ok(c) => Some(RustlsConfig::from_config(c)),
                Err(e) => {
                    tracing::error!(listener = name, error = %format!("{e:#}"), "TLS setup failed");
                    force = fallback(&last_good, &spec);
                    if force.is_none() {
                        wait_for_change(&mut spec_rx, &mut shutdown).await;
                    }
                    continue;
                }
            }
        } else {
            None
        };

        let handle = Handle::new();
        let app = make_router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let h = handle.clone();
        let addr = spec.addr;
        let mut task = tokio::spawn(async move {
            match tls_cfg {
                Some(c) => axum_server::bind_rustls(addr, c).handle(h).serve(app).await,
                None => axum_server::bind(addr).handle(h).serve(app).await,
            }
        });

        match handle.listening().await {
            Some(bound) => {
                state.set_bound(name, bound);
                last_good = Some(spec.clone());
                tracing::info!(listener = name, addr = %bound, tls = spec.tls.enabled, mtls = spec.mtls(), "listening");
            }
            None => {
                let err = task.await;
                tracing::error!(listener = name, addr = %spec.addr, result = ?err, "bind failed");
                force = fallback(&last_good, &spec);
                if force.is_some() {
                    tracing::warn!(listener = name, "restoring previous listener configuration");
                } else {
                    wait_for_change(&mut spec_rx, &mut shutdown).await;
                }
                continue;
            }
        }

        tokio::select! {
            res = &mut task => {
                tracing::error!(listener = name, result = ?res, "listener stopped unexpectedly");
                wait_for_change(&mut spec_rx, &mut shutdown).await;
            }
            _ = spec_rx.changed() => {
                tracing::info!(listener = name, "listener configuration changed; draining and restarting");
                handle.graceful_shutdown(Some(DRAIN));
                let _ = task.await;
            }
            _ = shutdown.changed() => {
                handle.graceful_shutdown(Some(DRAIN));
                let _ = task.await;
                return;
            }
        }
    }
}

fn fallback(last_good: &Option<ListenerSpec>, failed: &ListenerSpec) -> Option<ListenerSpec> {
    last_good.clone().filter(|g| g != failed)
}

async fn wait_for_change(
    rx: &mut watch::Receiver<Option<ListenerSpec>>,
    shutdown: &mut watch::Receiver<bool>,
) {
    tokio::select! {
        _ = rx.changed() => {}
        _ = shutdown.changed() => {}
    }
}
