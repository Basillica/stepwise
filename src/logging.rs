//! Structured logging with a runtime-adjustable level.

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    fmt, layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

use crate::config::{LogFormat, LoggingConfig};

pub struct LogHandle {
    filter: reload::Handle<EnvFilter, Registry>,
    _guard: Option<WorkerGuard>,
}

impl LogHandle {
    pub fn set_level(&self, level: &str) -> Result<()> {
        let f = EnvFilter::try_new(level)
            .with_context(|| format!("invalid log level/filter `{level}`"))?;
        self.filter.reload(f).context("reloading log filter")
    }
}

/// `RUST_LOG`, if set, overrides `logging.level` at startup.
pub fn init(cfg: &LoggingConfig) -> Result<LogHandle> {
    let initial = std::env::var("RUST_LOG").unwrap_or_else(|_| cfg.level.clone());
    let (filter, handle) = reload::Layer::new(
        EnvFilter::try_new(&initial)
            .with_context(|| format!("invalid log level/filter `{initial}`"))?,
    );

    let (writer, guard) = match &cfg.file {
        Some(path) => {
            let p = std::path::Path::new(path);
            let dir = p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            let name = p.file_name().context("logging.file has no file name")?;
            let (w, g) =
                tracing_appender::non_blocking(tracing_appender::rolling::never(dir, name));
            (Some(w), Some(g))
        }
        None => (None, None),
    };

    let layer: Box<dyn Layer<Registry> + Send + Sync> = match (&cfg.format, writer) {
        (LogFormat::Json, Some(w)) => fmt::layer().json().with_writer(w).with_ansi(false).boxed(),
        (LogFormat::Json, None) => fmt::layer().json().boxed(),
        (LogFormat::Pretty, Some(w)) => fmt::layer().with_writer(w).with_ansi(false).boxed(),
        (LogFormat::Pretty, None) => fmt::layer().boxed(),
    };
    // The reloadable filter wraps the fmt layer, so the handle type stays `Handle<EnvFilter, Registry>`.
    Registry::default()
        .with(layer.with_filter(filter))
        .try_init()
        .context("initialising logging")?;
    Ok(LogHandle {
        filter: handle,
        _guard: guard,
    })
}
