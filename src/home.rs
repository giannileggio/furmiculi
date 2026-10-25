use std::fs::TryLockError;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// How long a command waits for another `furmi` process to release the lock
/// before giving up, instead of hanging behind a stuck one.
const LOCK_TIMEOUT: Duration = Duration::from_secs(120);

/// The single folder holding everything furmiculi owns.
///
/// Resolved from `--home`, then `$FURMI_HOME`, then `~/.furmiculi`.
pub struct Home {
    root: PathBuf,
}

impl Home {
    pub fn resolve(flag: Option<PathBuf>) -> Result<Self> {
        let root = match flag.or_else(|| std::env::var_os("FURMI_HOME").map(PathBuf::from)) {
            Some(p) => std::path::absolute(expand_tilde(&p))?,
            None => default_root()?,
        };
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn tasks(&self) -> PathBuf {
        self.root.join("tasks")
    }

    pub fn schedules(&self) -> PathBuf {
        self.root.join("schedules")
    }

    pub fn archive(&self) -> PathBuf {
        self.root.join("archive")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn worktrees(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    /// Whether this is `~/.furmiculi`, which `furmi` finds without
    /// `--home` or `$FURMI_HOME`.
    pub fn is_default(&self) -> bool {
        default_root().is_ok_and(|d| d == self.root)
    }

    /// The `furmi-tasks` skill's folder.
    pub fn skill_dir(&self) -> PathBuf {
        self.root.join(".agents/skills/furmi-tasks")
    }

    pub fn dirs(&self) -> [PathBuf; 5] {
        [
            self.tasks(),
            self.schedules(),
            self.archive(),
            self.logs(),
            self.worktrees(),
        ]
    }

    /// Where `furmi run` appends its log (rotated when it grows large).
    pub fn runner_log(&self) -> PathBuf {
        self.root.join("runner.log")
    }

    /// Heartbeat written by `furmi run` every pass: pid, start and last tick.
    pub fn runner_state(&self) -> PathBuf {
        self.root.join("runner.json")
    }

    /// Where a backgrounded dashboard (`furmi start -d`, `furmi web -d`)
    /// appends its output: it has no terminal to print to.
    pub fn web_log(&self) -> PathBuf {
        self.root.join("web.log")
    }

    /// Where a serving dashboard records itself (pid and address), under a
    /// lock that dies with the process.
    pub fn web_lock(&self) -> PathBuf {
        self.root.join(".web.lock")
    }

    /// Records this process as the dashboard for this home, with `addr`, and
    /// holds the record until the returned file is dropped (or the process
    /// dies). Used to offer taking the address over: see
    /// [`dashboard`](Self::dashboard).
    pub fn claim_dashboard(&self, addr: &str) -> Result<std::fs::File> {
        let path = self.web_lock();
        // Like the runner's lock: not `File::create`, which would wipe the
        // running dashboard's entry just by trying to take the lock.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.try_lock()
            .map_err(|e| anyhow::anyhow!("locking {}: {e}", path.display()))?;
        file.set_len(0)
            .and_then(|()| write!(file, "{} {addr}", std::process::id()))
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(file)
    }

    /// The pid and address of the dashboard serving this home, if one is
    /// holding the lock now. A stale file from a crashed dashboard is never
    /// trusted: without the lock, its pid may long since have been reused.
    pub fn dashboard(&self) -> Option<(u32, String)> {
        let file = std::fs::File::open(self.web_lock()).ok()?;
        if !matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock)) {
            return None;
        }
        let text = std::fs::read_to_string(self.web_lock()).ok()?;
        let (pid, addr) = text.trim().split_once(' ')?;
        Some((pid.parse().ok()?, addr.to_string()))
    }

    /// Waits until no other `furmi` process is changing task files, and holds
    /// that lock until the returned file is dropped. Gives up after two
    /// minutes with an error, so a stuck process can't hang everything else.
    pub fn lock(&self) -> Result<std::fs::File> {
        let path = self.root.join(".lock");
        let file =
            std::fs::File::create(&path).with_context(|| format!("opening {}", path.display()))?;
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(TryLockError::WouldBlock) => bail!(
                    "{} is locked by another furmi process for over {}; if nothing is running, \
                     it may be stuck (see `furmi status`)",
                    path.display(),
                    humantime::format_duration(LOCK_TIMEOUT)
                ),
                Err(TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
    }

    /// Claims the right to be *the* `furmi run` for this home, until the
    /// returned file is dropped (or the process dies). Fails if another
    /// runner holds it. While held, the file contains this process's pid, so
    /// `furmi stop` knows who to ask to stop (see [`runner_pid`](Self::runner_pid)).
    pub fn claim_runner(&self) -> Result<std::fs::File> {
        let path = self.root.join(".runner.lock");
        // Not `File::create`: that would wipe a running runner's pid just
        // by trying (and failing) to take the lock.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {
                file.set_len(0)
                    .and_then(|()| write!(file, "{}", std::process::id()))
                    .with_context(|| format!("writing {}", path.display()))?;
                Ok(file)
            }
            Err(TryLockError::WouldBlock) => {
                bail!(
                    "another `furmi run` is already running for {}",
                    self.root.display()
                )
            }
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }
    }

    /// Whether some process currently holds [`claim_runner`](Self::claim_runner).
    pub fn runner_alive(&self) -> bool {
        let Ok(file) = std::fs::File::open(self.root.join(".runner.lock")) else {
            return false;
        };
        matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
    }

    /// The pid of the running runner, if there is one and it has written it
    /// yet. Only trusted while the lock is held, so a stale file from a dead
    /// runner (whose pid something else may have reused) is never returned.
    pub fn runner_pid(&self) -> Option<u32> {
        if !self.runner_alive() {
            return None;
        }
        std::fs::read_to_string(self.root.join(".runner.lock"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Fails with a helpful message if `furmi init` has not been run.
    pub fn ensure_initialized(&self) -> Result<()> {
        if !self.tasks().is_dir() {
            bail!(
                "{} is not initialized; run `furmi init` first",
                self.root.display()
            );
        }
        Ok(())
    }
}

fn default_root() -> Result<PathBuf> {
    Ok(home_dir()?.join(".furmiculi"))
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set")
}

/// Expands a leading `~` to `$HOME`. Other paths are returned unchanged.
pub fn expand_tilde(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => match home_dir() {
            Ok(home) => home.join(rest),
            Err(_) => path.to_path_buf(),
        },
        Err(_) => path.to_path_buf(),
    }
}

/// Inverse of [`expand_tilde`], for display.
pub fn contract_tilde(path: &Path) -> String {
    if let Ok(home) = home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return Path::new("~").join(rest).display().to_string();
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dashboard_records_itself_only_while_it_serves() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(home.dashboard(), None);

        let claim = home.claim_dashboard("0.0.0.0:7433").unwrap();
        assert_eq!(
            home.dashboard(),
            Some((std::process::id(), "0.0.0.0:7433".to_string()))
        );
        // Only the dashboard serving may hold the record.
        assert!(home.claim_dashboard("127.0.0.1:7433").is_err());

        // Once it's gone, the file is ignored: a crashed dashboard's pid may
        // have been reused by now.
        drop(claim);
        assert_eq!(home.dashboard(), None);
    }

    #[test]
    fn expand_and_contract_tilde_round_trip() {
        let home = home_dir().unwrap();

        assert_eq!(expand_tilde(Path::new("~/foo/bar")), home.join("foo/bar"));
        let absolute = Path::new("/not/under/home");
        assert_eq!(expand_tilde(absolute), absolute);

        assert_eq!(contract_tilde(&home.join("foo/bar")), "~/foo/bar");
        assert_eq!(contract_tilde(absolute), "/not/under/home");
    }

    #[test]
    fn resolve_uses_the_given_path_and_is_not_default() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();

        assert_eq!(home.root(), dir.path());
        assert!(!home.is_default());
        assert_eq!(home.config_path(), dir.path().join("config.toml"));
        assert_eq!(
            home.skill_dir(),
            dir.path().join(".agents/skills/furmi-tasks")
        );
        assert_eq!(
            home.dirs(),
            [
                dir.path().join("tasks"),
                dir.path().join("schedules"),
                dir.path().join("archive"),
                dir.path().join("logs"),
                dir.path().join("worktrees"),
            ]
        );
    }

    #[test]
    fn resolve_expands_a_leading_tilde() {
        let home = Home::resolve(Some(PathBuf::from("~/.furmiculi-test"))).unwrap();
        assert_eq!(home.root(), home_dir().unwrap().join(".furmiculi-test"));
    }

    #[test]
    fn the_runner_lock_holds_its_pid_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(home.runner_pid(), None);

        let claim = home.claim_runner().unwrap();
        assert_eq!(home.runner_pid(), Some(std::process::id()));

        // A second runner is refused, and doesn't disturb the first's pid.
        assert!(home.claim_runner().is_err());
        assert_eq!(home.runner_pid(), Some(std::process::id()));

        // Once released, the leftover file is not mistaken for a live runner.
        drop(claim);
        assert_eq!(home.runner_pid(), None);
    }

    #[test]
    fn ensure_initialized_checks_for_the_tasks_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();

        assert!(home.ensure_initialized().is_err());
        std::fs::create_dir_all(home.tasks()).unwrap();
        assert!(home.ensure_initialized().is_ok());
    }
}
