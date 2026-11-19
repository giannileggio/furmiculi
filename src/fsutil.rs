//! Crash-safe file writes and subprocess calls that can't hang forever.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// A temp file next to `path`, so the final rename stays on one filesystem.
/// Its name doesn't end in `.md`, so the folder scans never pick it up.
pub(crate) fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

fn write_temp(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let tmp = temp_path(path);
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    Ok(tmp)
}

/// Replaces `path` with `bytes` all at once: readers (and a crash at any
/// point) see either the old content or the new, never a truncated file.
pub fn write_atomic(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    let tmp = write_temp(path, bytes.as_ref())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::Error::new(e).context(format!("replacing {}", path.display()))
    })
}

/// Like [`write_atomic`], but fails (with `AlreadyExists` as the cause) if
/// `path` already exists. The file appears fully written or not at all.
pub fn create_atomic(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    let tmp = write_temp(path, bytes.as_ref())?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    linked.with_context(|| format!("creating {}", path.display()))
}

/// Runs `cmd` to completion, killing its whole process group if it takes
/// longer than `timeout`. stdin is closed, so nothing can wait on a prompt.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    };
    let out = drain(Box::new(child.stdout.take().expect("piped")));
    let err = drain(Box::new(child.stderr.take().expect("piped")));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            // SAFETY: plain libc call; the child leads its own process group.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    match status {
        Some(status) => Ok(Output {
            status,
            stdout,
            stderr,
        }),
        None => bail!("timed out after {}", crate::duration::format(timeout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a.md"]);
    }

    #[test]
    fn create_atomic_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        create_atomic(&path, "first").unwrap();
        let err = create_atomic(&path, "second").unwrap_err();
        let kind = err
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind);
        assert_eq!(kind, Some(std::io::ErrorKind::AlreadyExists));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A full disk (or any other failure while the temp file is
    /// written) can't be simulated portably, but planting a
    /// directory where the temp file would go fails the write the
    /// same way, before the rename: the original file must come
    /// through intact.
    #[test]
    fn a_failed_write_leaves_the_original_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        write_atomic(&path, "original").unwrap();
        let planted = temp_path(&path);
        std::fs::create_dir(&planted).unwrap();
        let err = write_atomic(&path, "replacement").unwrap_err();
        assert!(
            err.to_string().contains(&planted.display().to_string()),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        std::fs::remove_dir(&planted).unwrap();
    }

    #[test]
    fn a_failed_create_leaves_no_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        let planted = temp_path(&path);
        std::fs::create_dir(&planted).unwrap();
        assert!(create_atomic(&path, "first").is_err());
        assert!(!path.exists());
        std::fs::remove_dir(&planted).unwrap();
    }

    #[test]
    fn run_with_timeout_returns_output() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let out = run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn run_with_timeout_kills_a_hung_command_and_its_children() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & wait"]);
        let started = Instant::now();
        let err = run_with_timeout(cmd, Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
