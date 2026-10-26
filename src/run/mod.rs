//! `furmi run`: enqueue due schedules, poll running tasks, start pending ones.
//!
//! Each task runs in its own tmux window, named after the task id. The
//! window runs `furmi exec <id>`, which loads the task, runs the agent and
//! records its exit status in `logs/<id>.<attempt>.exit`. So the prompt
//! never passes through tmux's command line (which splits arguments ending
//! in `;` and caps message size), and the outcome doesn't depend on tmux's
//! own exit-status tracking (which lags behind `pane_dead`).
//!
//! Split by what the runner does: [`scheduler`] enqueues due schedules,
//! [`starting`] starts pending tasks (within the parallelism limits),
//! [`reaping`] reaps running ones and
//! [`finishing`] closes them out, and [`mod@exec`] is the per-task process that
//! runs the agent (parsing its output for usage in [`usage`]). [`mod@log`] is
//! the runner log.

mod exec;
mod finishing;
mod log;
mod reaping;
mod scheduler;
mod starting;
mod usage;

pub use exec::exec;
pub use finishing::{archive, complete};
pub use log::log;
pub use reaping::{save_log, window_of};
pub use scheduler::failure_streak;
pub(crate) use usage::model_from_log;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::cmd::md_files;
use crate::config::Config;
use crate::fsutil;
use crate::home::Home;
use crate::task::{Status, Task, now};

use self::finishing::finalize;
use self::log::LOG_FILE;
use self::reaping::poll_running;
use self::scheduler::enqueue_schedules;
use self::starting::start_pending;

/// Set from the SIGTERM/SIGINT handler (see [`install_shutdown_handler`]).
/// The runner checks it between passes, so a signal can't tear down a
/// half-finished tick or the tmux windows running tasks live in.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Turns SIGTERM/SIGINT into a request to stop after the current pass.
/// Without it the process dies mid-tick, which can leave a task file
/// written but not yet archived. Running tasks keep going in their tmux
/// windows either way; the next `furmi run` picks them up.
pub fn install_shutdown_handler() {
    // SAFETY: `on_shutdown` only stores to an atomic, which is
    // async-signal-safe; the previous disposition was the default.
    unsafe {
        let handler = on_shutdown as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

/// The first signal asks for a clean stop. A second one means the pass is
/// stuck (e.g. a slow push) and the user wants out now: exit immediately
/// (`_exit` is async-signal-safe). Running tasks live in tmux and survive.
extern "C" fn on_shutdown(_signal: libc::c_int) {
    if SHUTDOWN.swap(true, Ordering::Relaxed) {
        // SAFETY: `_exit` is async-signal-safe and never returns.
        unsafe { libc::_exit(130) }
    }
}

/// Whether a stop has been requested (SIGTERM/SIGINT).
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

pub fn run(home: &Home, once: bool) -> Result<()> {
    home.ensure_initialized()?;
    let _runner = home.claim_runner()?;
    install_shutdown_handler();
    let _ = LOG_FILE.set(home.runner_log());
    let mut config = Config::load(&home.config_path())?;
    let mut state = RunnerState {
        pid: std::process::id(),
        started_at: now(),
        last_tick_at: None,
        last_error: None,
    };
    if !once {
        log(&format!(
            "watching {} every {}",
            home.root().display(),
            humantime::format_duration(config.runner.poll_interval)
        ));
    }
    loop {
        let result = tick(home, &config);
        state.last_tick_at = Some(now());
        state.last_error = result.as_ref().err().map(|e| format!("{e:#}"));
        if !once {
            state.save(home);
        }
        if let Err(e) = result {
            if once {
                return Err(e);
            }
            log(&format!("error: {e:#}"));
        }
        if once {
            return Ok(());
        }
        if !sleep_until_tick(config.runner.poll_interval) {
            log(
                "stopping on request: running tasks keep going in their tmux windows; \
                 `furmi run` will pick them up again",
            );
            return Ok(());
        }
        match Config::load(&home.config_path()) {
            Ok(c) => config = c,
            Err(e) => log(&format!("keeping the previous config: {e:#}")),
        }
    }
}

/// Sleeps up to `duration`, returning `false` as soon as a stop is
/// requested. Short slices keep a long poll interval from delaying a
/// signal-driven stop.
fn sleep_until_tick(duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if shutdown_requested() {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
    !shutdown_requested()
}

/// The runner's heartbeat, in `runner.json`: lets `furmi status` tell a live
/// runner from a dead or wedged one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerState {
    pub pid: u32,
    pub started_at: DateTime<FixedOffset>,
    pub last_tick_at: Option<DateTime<FixedOffset>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl RunnerState {
    fn save(&self, home: &Home) {
        let written = serde_json::to_string(self)
            .map_err(anyhow::Error::from)
            .and_then(|json| fsutil::write_atomic(&home.runner_state(), json));
        if let Err(e) = written {
            log(&format!("could not write the heartbeat: {e:#}"));
        }
    }

    pub fn load(home: &Home) -> Option<Self> {
        let text = std::fs::read_to_string(home.runner_state()).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// One pass. The home lock is held while task files change, but not while
/// `on_finish` runs (it can talk to the network): successful tasks are
/// collected under the lock and finalized after it's released.
fn tick(home: &Home, config: &Config) -> Result<()> {
    let succeeded = {
        let _lock = home.lock()?;
        let now = now();
        enqueue_schedules(home, config, now)?;
        let succeeded = poll_running(home, config, now)?;
        start_pending(home, config, now)?;
        succeeded
    };
    for (path, task) in succeeded {
        let id = task.id.clone();
        if let Err(e) = finalize(home, config, &path, task) {
            log(&format!("finishing {id}: {e:#}"));
        }
    }
    Ok(())
}

// --- helpers -------------------------------------------------------------------

pub(super) fn status_file(home: &Home, id: &str, attempt: u32) -> PathBuf {
    home.logs().join(format!("{id}.{attempt}.exit"))
}

pub(super) fn log_file(home: &Home, id: &str, attempt: u32) -> PathBuf {
    home.logs().join(format!("{id}.{attempt}.log"))
}

pub(super) fn usage_file(home: &Home, id: &str, attempt: u32) -> PathBuf {
    home.logs().join(format!("{id}.{attempt}.usage"))
}

pub(super) fn load_tasks(home: &Home, status: Status) -> Result<Vec<(PathBuf, Task)>> {
    let mut out = Vec::new();
    for path in md_files(&home.tasks())? {
        match Task::load(&path) {
            Ok(t) if t.status == status => out.push((path, t)),
            Ok(_) => {}
            Err(e) => log(&format!("skipping {}: {e:#}", path.display())),
        }
    }
    Ok(out)
}
