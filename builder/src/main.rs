use std::sync::Arc;

use anyhow::{Context, Result};
use stepwise_builder::{router, Builder, Config};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cfg = Config::from_env()?;
    let addr = std::env::var("BUILDER_LISTEN").unwrap_or_else(|_| "0.0.0.0:7070".into());
    let builder: Arc<Builder> = Builder::new(cfg).await?;
    tracing::info!(toolchains = ?builder.toolchains, "builder ready");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, router(builder))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
