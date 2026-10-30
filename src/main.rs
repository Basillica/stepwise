use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use stepwise::{
    compile,
    config::Config,
    control, dataplane,
    hooks::HookRegistry,
    logging,
    metrics::Metrics,
    ratelimit::RateLimiter,
    state::{AppState, LogReloadFn},
    supervisor,
};
use tokio::sync::watch;

const USAGE: &str = "usage: stepwise [run] <config.yaml>\n       stepwise check <config.yaml>\n       stepwise --version";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, path) = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--version" | "-V"] => {
            println!("stepwise {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        ["check", p] => ("check", PathBuf::from(p)),
        ["run", p] | [p] if !p.starts_with('-') => ("run", PathBuf::from(p)),
        [] => ("run", PathBuf::from("config.yaml")),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let config = Config::load(&path)?;
    if cmd == "check" {
        return check(config);
    }

    let log = Arc::new(logging::init(&config.logging)?);
    for key in config.extra.keys() {
        tracing::warn!(section = %key, "config section is not implemented yet and is ignored");
    }
    let log_for_reload = log.clone();
    let reload: LogReloadFn = Box::new(move |l| log_for_reload.set_level(&l.level));

    let state = AppState::new(
        config,
        Some(path),
        HookRegistry::with_builtins(),
        Some(reload),
    )
    .context("starting server")?;

    if let Some(db) = &state.db {
        db.ping()
            .await
            .context("cannot reach the database (check database.url / database.path)")?;
        tracing::info!(backend = ?state.config().database.db_type, "database ready");
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    let data = tokio::spawn(supervisor::run_listener(
        "data_plane",
        state.clone(),
        state.data_listener.subscribe(),
        dataplane::router,
        stop_rx.clone(),
    ));
    let ctl = tokio::spawn(supervisor::run_listener(
        "control_plane",
        state.clone(),
        state.control_listener.subscribe(),
        control::router,
        stop_rx,
    ));

    shutdown_signal().await;
    tracing::info!("shutting down");
    let _ = stop_tx.send(true);
    let _ = tokio::join!(data, ctl);
    Ok(())
}

/// Compile everything and print the route table. Non-zero exit if anything is wrong.
fn check(config: Config) -> Result<()> {
    let rt = compile::build_runtime(
        Arc::new(config),
        &HookRegistry::with_builtins(),
        Arc::new(RateLimiter::new()),
        Arc::new(Metrics::default()),
        None,
    )?;
    println!(
        "{} v{}: {} routes",
        rt.spec_title,
        rt.spec_version,
        rt.table.routes().len()
    );
    for r in rt.table.routes() {
        let hooks: usize = r
            .steps
            .values()
            .map(|p| p.before.len() + p.after.len() + usize::from(p.replace.is_some()))
            .sum();
        println!(
            "  {:<28} -> {}{}{}{}{}",
            r.id,
            r.response.status,
            if r.protected { "  [auth]" } else { "" },
            if r.rate.is_some() {
                "  [rate-limited]"
            } else {
                ""
            },
            r.crud
                .as_ref()
                .map(|c| format!("  [db: {:?} {}]", c.op, c.collection))
                .unwrap_or_default(),
            if hooks > 0 {
                format!("  [{hooks} hooks]")
            } else {
                String::new()
            },
        );
    }
    println!("ok");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
