use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use crate::home::Home;

const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);
const CHECK_TIMEOUT: Duration = Duration::from_millis(1500);

/// Prints a one-line notice on stderr if a newer release is out.
///
/// Only fires for installs done through the generated installer, which
/// writes an install receipt `is_update_needed_sync` reads; a `cargo
/// install` or from-source build has nothing to compare against and stays
/// silent. Cached to at most once a day, and bounded to `CHECK_TIMEOUT` so
/// a dead network never adds noticeable latency to a command.
pub fn check(home: &Home) {
    if std::env::var_os("FURMI_NO_UPDATE_CHECK").is_some() || !home.root().is_dir() || !due(home) {
        return;
    }

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // axoupdater's own error type trips clippy::result_large_err; it
        // allows the same lint internally for the same reason.
        #[allow(clippy::result_large_err)]
        fn update_needed() -> axoupdater::AxoupdateResult<bool> {
            crate::update::updater()
                .load_receipt()?
                .is_update_needed_sync()
        }
        let _ = tx.send(update_needed().unwrap_or(false));
    });

    if let Ok(true) = rx.recv_timeout(CHECK_TIMEOUT) {
        eprintln!(
            "A new version of furmi is available. Run `furmi update` to install it, or see \
             https://github.com/giannileggio/furmiculi/releases."
        );
    }
}

/// Whether it's been at least `CHECK_INTERVAL` since the last check.
/// Touches the marker right away so a slow or failing check isn't retried
/// on every invocation in the meantime.
fn due(home: &Home) -> bool {
    let marker = home.root().join(".update-checked");
    let stale = match std::fs::metadata(&marker).and_then(|m| m.modified()) {
        Ok(modified) => SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age >= CHECK_INTERVAL),
        Err(_) => true,
    };
    if stale {
        let _ = std::fs::write(&marker, b"");
    }
    stale
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::Home;

    fn home() -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        (dir, home)
    }

    #[test]
    fn due_once_a_day_and_touches_the_marker() {
        let (_dir, home) = home();
        let marker = home.root().join(".update-checked");

        assert!(due(&home), "no marker yet: due");
        assert!(marker.is_file());
        assert!(!due(&home), "just touched: not due again");

        let old = SystemTime::now() - CHECK_INTERVAL - Duration::from_secs(1);
        std::fs::File::open(&marker)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(due(&home), "marker older than the interval: due again");
    }
}
