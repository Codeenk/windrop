//! Logging setup.
//!
//! Every record goes to `<logs_dir>/windrop.log`, which is what the GUI's log
//! pane and `windrop logs` read. The terminal gets a *second*, independently
//! filtered copy: a command-line tool that prints its internal progress by
//! default is unusable in a pipeline, while a log file that omits it is useless
//! for diagnosis. So the file keeps everything and the terminal shows what was
//! asked for — `warn` by default, more with `-v`.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

use crate::Result;

/// Handle that keeps the logging subsystem alive.
pub struct LogGuard {
    path: std::path::PathBuf,
}

impl LogGuard {
    /// Where the log is being written.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Install a global tracing subscriber writing to stderr and to
/// `<logs_dir>/windrop.log`.
///
/// Returns `None` when a subscriber is already installed (e.g. a test harness
/// called this twice); callers should treat that as "logging already works".
pub fn init(level: &str, logs_dir: &Path) -> Result<Option<LogGuard>> {
    init_split(level, level, logs_dir)
}

/// Install a subscriber with different verbosity for the log file and for the
/// terminal.
///
/// `level` sets what the file records; `terminal_level` sets what reaches
/// stderr. `RUST_LOG` overrides both, which is the escape hatch every Rust
/// program should offer.
///
/// Returns `None` when a subscriber is already installed (e.g. a test harness
/// called this twice); callers should treat that as "logging already works".
pub fn init_split(level: &str, terminal_level: &str, logs_dir: &Path) -> Result<Option<LogGuard>> {
    std::fs::create_dir_all(logs_dir)?;
    let path = logs_dir.join("windrop.log");
    let file = OpenOptions::new().create(true).append(true).open(&path)?;

    let override_filter = EnvFilter::try_from_default_env().ok();
    let file_filter = override_filter
        .clone()
        .or_else(|| EnvFilter::try_new(level).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let terminal_filter = override_filter.unwrap_or_else(|| {
        // An unusable directive must not take logging down: fall back politely.
        EnvFilter::try_new(terminal_level).unwrap_or_else(|_| EnvFilter::new("warn"))
    });

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(TeeMakeWriter {
            file: Arc::new(Mutex::new(file)),
            also_stderr: false,
        })
        .with_filter(file_filter);
    let terminal_layer = fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(io::stderr)
        .with_filter(terminal_filter);

    // `try_init` fails when a subscriber already exists; that is fine.
    if tracing_subscriber::registry()
        .with(file_layer)
        .with(terminal_layer)
        .try_init()
        .is_err()
    {
        return Ok(None);
    }
    Ok(Some(LogGuard { path }))
}

/// Silences logging, for CLI invocations that only care about parsed output.
pub fn init_quiet() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("error"))
        .with_writer(io::sink)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// A `MakeWriter` that duplicates every record to a file and optionally stderr.
#[derive(Clone)]
struct TeeMakeWriter {
    file: Arc<Mutex<std::fs::File>>,
    also_stderr: bool,
}

struct TeeWriter {
    file: Arc<Mutex<std::fs::File>>,
    also_stderr: bool,
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            file: Arc::clone(&self.file),
            also_stderr: self.also_stderr,
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.also_stderr {
            let _ = io::stderr().write_all(buf);
        }
        match self.file.lock() {
            Ok(mut f) => {
                f.write_all(buf)?;
                Ok(buf.len())
            }
            // A poisoned lock must not take the logger down with it.
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.also_stderr {
            let _ = io::stderr().flush();
        }
        if let Ok(mut f) = self.file.lock() {
            f.flush()?;
        }
        Ok(())
    }
}

/// Read the tail of the log file, for the GUI log viewer.
pub fn read_log_tail(log_file: &Path, max_lines: usize) -> String {
    let text = std::fs::read_to_string(log_file).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tee_writer_always_reaches_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("windrop.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let w = TeeMakeWriter {
            file: Arc::new(Mutex::new(file)),
            also_stderr: false,
        };
        let mut writer = w.make_writer();
        writer.write_all(b"hello from the tee\n").unwrap();
        writer.flush().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "hello from the tee\n"
        );
    }

    #[test]
    fn the_file_layer_never_writes_to_stderr() {
        // The two destinations are separate on purpose: a test that asserts on
        // captured stdout must not be disturbed by the log.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("windrop.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let w = TeeMakeWriter {
            file: Arc::new(Mutex::new(file)),
            also_stderr: false,
        };
        let mut writer = w.make_writer();
        writer.write_all(b"file only\n").unwrap();
        writer.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "file only\n");
    }

    #[test]
    fn read_log_tail_returns_the_last_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("windrop.log");
        std::fs::write(&path, "1\n2\n3\n4\n5\n").unwrap();
        assert_eq!(read_log_tail(&path, 2), "4\n5");
        assert_eq!(read_log_tail(&path, 99), "1\n2\n3\n4\n5");
    }

    #[test]
    fn read_log_tail_on_missing_file_is_empty() {
        assert_eq!(read_log_tail(Path::new("/nonexistent/log"), 10), "");
    }
}
