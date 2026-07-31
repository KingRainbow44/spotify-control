//! Tracing setup.
//!
//! The daemon runs from a logon task with no console attached, so anything it
//! writes to stdout is simply lost — an expired token or a rejected binding
//! would fail in total silence. It therefore also logs to a rotating file.

use anyhow::{Context, Result};
use std::path::Path;
use tracing_appender::rolling::{Builder, RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const FILE_PREFIX: &str = "spotify-control";
const FILE_SUFFIX: &str = "log";

/// A week is enough to explain "it stopped working sometime last week" without
/// letting the directory grow forever.
const KEEP_FILES: usize = 7;

fn filter(verbose: bool) -> EnvFilter {
    let default = if verbose {
        "spotify_control=debug"
    } else {
        "spotify_control=info"
    };
    EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into())
}

/// Writes synchronously, deliberately: `tracing_appender::non_blocking` hands
/// lines to a background thread, and the daemon's fatal path calls
/// `process::exit`, which kills that thread before it flushes the one line that
/// explains what went wrong. This logs a few lines per keypress, so the cost of
/// writing them inline is nothing.
fn file_appender(dir: &Path) -> Result<RollingFileAppender> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("could not create the log directory {}", dir.display()))?;

    Builder::new()
        .filename_prefix(FILE_PREFIX)
        .filename_suffix(FILE_SUFFIX)
        .rotation(Rotation::DAILY)
        .max_log_files(KEEP_FILES)
        .build(dir)
        .with_context(|| format!("could not open a log file in {}", dir.display()))
}

/// Console only — enough for the short-lived subcommands. Diagnostics go to
/// stderr so they don't interleave into `status`'s report on stdout.
pub fn init_cli(verbose: bool) {
    tracing_subscriber::fmt()
        .with_env_filter(filter(verbose))
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

/// Console *and* file, for the daemon. A log file we can't open is not worth
/// losing the hotkeys over, so that degrades to console-only rather than
/// failing the daemon outright.
pub fn init_daemon(verbose: bool) {
    let opened = crate::config::log_dir().and_then(|dir| Ok((file_appender(&dir)?, dir)));

    let (appender, dir) = match opened {
        Ok(pair) => pair,
        Err(e) => {
            init_cli(verbose);
            tracing::warn!("logging to file is unavailable, console only: {e:#}");
            return;
        }
    };

    tracing_subscriber::registry()
        .with(filter(verbose))
        // No ANSI in the file — escape codes make it unreadable in Notepad.
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(appender),
        )
        // Writes fail harmlessly when the GUI-subsystem binary has no console.
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_writer(std::io::stderr),
        )
        .init();

    tracing::info!("logging to {}", dir.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive one line through the appender and return what landed in `dir`.
    /// Nothing is dropped or flushed first — that's the point.
    fn log_one(dir: &Path, message: &str) -> Vec<(String, String)> {
        let appender = file_appender(dir).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(appender)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || tracing::info!("{message}"));

        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, std::fs::read_to_string(&path).unwrap_or_default())
            })
            .collect()
    }

    #[test]
    fn a_line_is_on_disk_before_anything_is_dropped() {
        // The daemon exits the process from its worker thread on a fatal error.
        // A buffered writer would lose exactly the line that explains why.
        let dir = tempfile::tempdir().unwrap();
        let files = log_one(dir.path(), "hello from the daemon");
        assert!(
            files
                .iter()
                .any(|(_, body)| body.contains("hello from the daemon")),
            "log line never reached disk: {files:?}"
        );
    }

    #[test]
    fn log_files_are_named_recognisably() {
        let dir = tempfile::tempdir().unwrap();
        let files = log_one(dir.path(), "x");
        let (name, _) = files.first().expect("no log file was created");
        assert!(name.starts_with(FILE_PREFIX), "got {name}");
        assert!(name.ends_with(FILE_SUFFIX), "got {name}");
    }

    #[test]
    fn missing_log_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        file_appender(&nested).unwrap();
        assert!(nested.is_dir());
    }
}
