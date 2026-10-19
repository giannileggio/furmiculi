//! The path to the running `furmi` binary.
//!
//! It has to be resolved once, at startup. On Linux `/proc/self/exe` (and so
//! `current_exe`) gains a ` (deleted)` suffix as soon as the file is replaced
//! or unlinked under a running process, which is exactly what `cargo build`
//! next to a live `furmi run` does. A stale path there made every task window
//! fail to start, silently, until the runner was restarted.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Where this binary lives, recorded before anything can replace it.
static EXE: OnceLock<PathBuf> = OnceLock::new();

/// Record the binary's path. Call once, first thing in `main`.
pub fn remember() {
    if let Ok(path) = std::env::current_exe() {
        let _ = EXE.set(normalize(path));
    }
}

/// The binary's path: the one recorded at startup, or `current_exe` with the
/// ` (deleted)` marker stripped.
pub fn path() -> std::io::Result<PathBuf> {
    match EXE.get() {
        Some(path) => Ok(path.clone()),
        None => Ok(normalize(std::env::current_exe()?)),
    }
}

/// Drops the ` (deleted)` suffix Linux appends once the file is unlinked.
fn normalize(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|p| p.strip_suffix(" (deleted)")) {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_deleted_marker() {
        assert_eq!(
            normalize(PathBuf::from("/bin/furmi (deleted)")),
            PathBuf::from("/bin/furmi")
        );
        assert_eq!(
            normalize(PathBuf::from("/bin/furmi")),
            PathBuf::from("/bin/furmi")
        );
    }

    #[test]
    fn path_comes_from_startup() {
        remember();
        assert!(EXE.get().is_some());
        let path = path().unwrap();
        assert!(!path.to_string_lossy().contains("(deleted)"));
    }
}
