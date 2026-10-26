//! The runner's log: printed, and also appended to `runner.log`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::Local;

/// Where [`log`] also appends, once `furmi run` has set it.
pub(super) static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
/// The log file is moved to `.1` (replacing an older one) beyond this size.
const MAX_RUNNER_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub fn log(msg: &str) {
    let line = format!("{} {msg}", Local::now().format("%Y-%m-%d %H:%M:%S"));
    println!("{line}");
    if let Some(path) = LOG_FILE.get() {
        append_log_line(path, &line);
    }
}

fn append_log_line(path: &Path, line: &str) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_RUNNER_LOG_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}
