//! Optional file logging sink.
//!
//! The default `tracing` setup writes to the terminal (stdout), where the
//! interesting lines are easily lost in scrollback. This module adds an
//! opt-in file sink that runs *alongside* the terminal layer, with its own
//! (typically more verbose) filter so detailed traces can be captured to disk
//! without flooding the terminal.
//!
//! Activation (env vars, read once at startup):
//!   - `VK_LOG_FILE=<path>`   Write logs to this exact file (created if needed,
//!                            appended to otherwise).
//!   - `VK_LOG_TO_FILE=1`     Write logs to `<asset_dir>/logs/vibe-kanban.log`.
//!                            Ignored when `VK_LOG_FILE` is set.
//!   - `VK_LOG_FILTER=<dirs>` Override the file sink's `EnvFilter` directives.
//!                            Defaults to `DEFAULT_FILE_FILTER` below, which is
//!                            tuned to capture the diff stream in detail.
//!
//! When none of the activation vars are set, [`file_layer`] returns `(None,
//! None)` and logging behaviour is unchanged.

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Layer, fmt, registry::LookupSpan};

use crate::assets::asset_dir;

/// Verbose default filter for the file sink: everything at info, the diff
/// stream (the main suspect for stale-diff debugging) at trace.
pub const DEFAULT_FILE_FILTER: &str = "info,services=debug,server=debug,local_deployment=debug,services::services::diff_stream=trace,services::services::filesystem_watcher=trace";

/// Build the optional file-logging layer from environment configuration.
///
/// Returns the layer (to add to the subscriber registry) plus a [`WorkerGuard`]
/// that **must be kept alive for the lifetime of the process** — dropping it
/// flushes and shuts down the background writer thread, so bind it to a
/// long-lived variable in `main` (e.g. `let _guard = ...;`).
pub fn file_layer<S>() -> (Option<Box<dyn Layer<S> + Send + Sync>>, Option<WorkerGuard>)
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let Some(path) = resolve_log_path() else {
        return (None, None);
    };

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("[logging] failed to create log dir {parent:?}: {e}");
        return (None, None);
    }

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[logging] failed to open log file {path:?}: {e}");
            return (None, None);
        }
    };

    let filter_str =
        std::env::var("VK_LOG_FILTER").unwrap_or_else(|_| DEFAULT_FILE_FILTER.to_string());
    let env_filter = match EnvFilter::try_new(&filter_str) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[logging] invalid VK_LOG_FILTER {filter_str:?}: {e}");
            return (None, None);
        }
    };

    let (writer, guard) = tracing_appender::non_blocking(file);
    let layer = fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(env_filter)
        .boxed();

    eprintln!("[logging] writing logs to {path:?} (filter: {filter_str})");
    (Some(layer), Some(guard))
}

fn resolve_log_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("VK_LOG_FILE") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    let to_file = std::env::var("VK_LOG_TO_FILE")
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false);

    if to_file {
        return Some(asset_dir().join("logs").join("vibe-kanban.log"));
    }

    None
}
