//! Changing a task's status by hand: `furmi done|fail|cancel|retry|attach`.
//!
//! These hold the home lock while they touch task files, so they never race
//! a `tick` of `furmi run`. The id defaults to `$FURMI_TASK_ID`, which `furmi run`
//! sets in each task's window, so `furmi done` works from inside a session.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::home::Home;
use crate::run;
use crate::spec::validate_id;
use crate::task::{Status, Task, now};
use crate::tmux;

/// How long `furmi stop` waits for the runner to exit, and `start_runner` for
/// one to come up, before giving up.
const RUNNER_WAIT: Duration = Duration::from_secs(10);

fn wait_for(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Starts `furmi run` in the background, detached from this process and its
/// terminal, so it keeps going after the dashboard or shell that started it
/// is closed. Returns `false` if a runner was already running.
pub fn start_runner(home: &Home) -> Result<bool> {
    if home.runner_alive() {
        return Ok(false);
    }
    let exe = crate::exe::path().context("finding the furmi binary to start the runner")?;
    let mut child = Command::new(exe)
        .arg("--home")
        .arg(home.root())
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group: a Ctrl-C in our terminal isn't meant for it.
        .process_group(0)
        .spawn()
        .context("starting the runner")?;
    let mut exited = None;
    let up = wait_for(RUNNER_WAIT, || {
        if home.runner_alive() {
            return true;
        }
        exited = child.try_wait().ok().flatten();
        exited.is_some()
    });
    if let Some(status) = exited {
        bail!(
            "the runner exited right away ({status}); see {}",
            home.runner_log().display()
        );
    }
    if !up {
        let _ = child.kill();
        bail!(
            "the runner didn't come up within {}s",
            RUNNER_WAIT.as_secs()
        );
    }
    // Reap it when it eventually exits, instead of leaving a zombie behind
    // in a long-lived dashboard.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(true)
}

/// Asks the running runner to exit. Tasks already running keep going in
/// their tmux windows; the next runner picks them back up. Returns the
/// runner's pid, or `None` if none was running.
pub fn stop_runner(home: &Home) -> Result<Option<u32>> {
    if !home.runner_alive() {
        return Ok(None);
    }
    // A runner that's only just started writes its pid a moment after
    // taking the lock.
    let mut pid = None;
    wait_for(Duration::from_secs(2), || {
        pid = home.runner_pid();
        pid.is_some() || !home.runner_alive()
    });
    let Some(pid) = pid else {
        // It exited by itself while we were looking.
        return Ok(None);
    };
    let pid_t = i32::try_from(pid).context("the runner's pid is out of range")?;
    // SAFETY: plain kill(2) on a pid read from the lock the runner holds.
    if unsafe { libc::kill(pid_t, libc::SIGTERM) } != 0 {
        let err = std::io::Error::last_os_error();
        bail!("couldn't stop the runner (pid {pid}): {err}");
    }
    if !wait_for(RUNNER_WAIT, || !home.runner_alive()) {
        bail!(
            "the runner (pid {pid}) is still running {}s after being asked to stop; \
             `kill -9 {pid}` will force it",
            RUNNER_WAIT.as_secs()
        );
    }
    Ok(Some(pid))
}

/// Runs `on_finish` and archives the task as done. Works on running tasks
/// (ending their session), on `needs_review` ones, and on failed ones, e.g.
/// after fixing whatever made `on_finish` fail.
///
/// The home lock is held to read the task and end its session, then released
/// for `on_finish` (a push or a PR, which can take minutes) so it can't stall
/// the runner, the dashboard or `furmi add`; `run::complete` takes it again to
/// archive. A `running` task is first marked `needs_review`: with its window
/// gone, the runner would otherwise see it as interrupted and requeue it
/// while `on_finish` is still going. If `furmi done` dies midway the task is
/// left `needs_review`, and `furmi done` can simply be run again.
pub fn done(home: &Home, id: Option<String>) -> Result<()> {
    let (config, id) = setup(home, id)?;
    let (path, task, deferred) = {
        let _lock = home.lock()?;
        let (path, mut task) = find(home, &id)?;
        let deferred = match task.status {
            Status::Running => {
                let deferred = end_session(home, &config, &task)?;
                task.status = Status::NeedsReview;
                task.tmux_window = None;
                task.save(&path)?;
                deferred
            }
            Status::NeedsReview | Status::Failed => None,
            s => bail!(
                "`{id}` is {}; only running, needs_review or failed tasks can be marked done",
                s.as_str()
            ),
        };
        (path, task, deferred)
    };
    let finished = run::complete(home, &config, &path, task, now());
    // From inside the task's window the kill waits until `on_finish` is
    // over, and must happen even if that (or the archiving) failed: a
    // failure there must not leave the window running.
    let killed = kill_deferred(deferred);
    finished?;
    killed?;
    Ok(())
}

/// Archives the task as failed, without retrying.
pub fn fail(home: &Home, id: Option<String>, reason: Option<String>) -> Result<()> {
    let (config, id) = setup(home, id)?;
    let lock = home.lock()?;
    let (path, mut task) = find(home, &id)?;
    let deferred = match task.status {
        Status::Running => end_session(home, &config, &task)?,
        Status::NeedsReview => None,
        s => bail!(
            "`{id}` is {}; only running or needs_review tasks can be marked failed",
            s.as_str()
        ),
    };
    task.status = Status::Failed;
    task.error = Some(reason.unwrap_or_else(|| "marked failed with `furmi fail`".into()));
    run::archive(home, &path, task, now())?;
    drop(lock);
    kill_deferred(deferred)
}

/// Archives a pending or running task as cancelled. `on_finish` doesn't run.
pub fn cancel(home: &Home, id: Option<String>) -> Result<()> {
    let (config, id) = setup(home, id)?;
    let lock = home.lock()?;
    let (path, mut task) = find(home, &id)?;
    let deferred = match task.status {
        Status::Running => end_session(home, &config, &task)?,
        Status::Pending => None,
        s => bail!(
            "`{id}` is {}; only pending or running tasks can be cancelled",
            s.as_str()
        ),
    };
    task.status = Status::Cancelled;
    run::archive(home, &path, task, now())?;
    drop(lock);
    kill_deferred(deferred)
}

/// Moves a failed, cancelled or `needs_review` task back to the queue, to
/// start as soon as possible in the same worktree. `attempts` keeps
/// counting, so logs aren't overwritten.
pub fn retry(home: &Home, id: &str) -> Result<()> {
    home.ensure_initialized()?;
    let _lock = home.lock()?;
    let (path, mut task) = find(home, id)?;
    if !matches!(
        task.status,
        Status::Failed | Status::Cancelled | Status::NeedsReview
    ) {
        bail!(
            "`{id}` is {}; only failed, cancelled or needs_review tasks can be retried",
            task.status.as_str()
        );
    }
    task.status = Status::Pending;
    task.scheduled_at = None;
    task.started_at = None;
    task.finished_at = None;
    task.exit_code = None;
    task.tmux_window = None;
    task.error = None;
    let dest = home.tasks().join(format!("{id}.md"));
    task.create(&dest)?;
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    run::log(&format!("{id} requeued"));
    Ok(())
}

/// Attaches to a running task's window, or to the whole session.
pub fn attach(home: &Home, id: Option<String>) -> Result<()> {
    home.ensure_initialized()?;
    let config = Config::load(&home.config_path())?;
    let Some(id) = id else {
        let session = &config.runner.tmux_session;
        if !tmux::session_exists(session)? {
            bail!("no tmux session `{session}`: nothing is running");
        }
        return Err(tmux::attach(session, None));
    };
    let (_, task) = find(home, &id)?;
    if task.status != Status::Running {
        bail!("`{id}` is {}, not running", task.status.as_str());
    }
    let (session, window) = run::window_of(&task, &config);
    Err(tmux::attach(&session, Some(&window)))
}

/// A task's captured output: the live tmux pane if it's still running,
/// else the saved log for an attempt (the latest, by default).
pub fn logs(home: &Home, id: &str, attempt: Option<u32>) -> Result<String> {
    home.ensure_initialized()?;
    let config = Config::load(&home.config_path())?;
    let (_, task) = find(home, id)?;
    if task.attempts == 0 {
        bail!("`{id}` hasn't started yet");
    }
    let attempt = attempt.unwrap_or(task.attempts);
    if attempt == 0 || attempt > task.attempts {
        bail!("`{id}` has {} attempt(s) so far", task.attempts);
    }
    if task.status == Status::Running && attempt == task.attempts {
        let (session, window) = run::window_of(&task, &config);
        if tmux::session_exists(&session)? {
            return tmux::capture(&session, &window);
        }
    }
    let path = home.logs().join(format!("{id}.{attempt}.log"));
    std::fs::read_to_string(&path)
        .with_context(|| format!("no log saved for `{id}` attempt {attempt}"))
}

fn setup(home: &Home, id: Option<String>) -> Result<(Config, String)> {
    home.ensure_initialized()?;
    let config = Config::load(&home.config_path())?;
    let id = id
        .or_else(|| std::env::var("FURMI_TASK_ID").ok())
        .context("no task id: pass one, or run this inside a task's window")?;
    Ok((config, id))
}

/// The task's file, in `tasks/` or else `archive/`.
fn find(home: &Home, id: &str) -> Result<(PathBuf, Task)> {
    validate_id(id).map_err(anyhow::Error::msg)?;
    for dir in [home.tasks(), home.archive()] {
        let path = dir.join(format!("{id}.md"));
        if path.exists() {
            let task = Task::load(&path)?;
            return Ok((path, task));
        }
    }
    bail!("no task `{id}` in tasks/ or archive/")
}

/// How long a stopped agent gets to exit after SIGTERM before SIGKILL.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

type Window = (String, String);

/// Saves a running task's output and stops its agent and window, so the agent stops
/// before `on_finish` touches its work. From inside that window, killing it
/// would kill this command too, so the kill is returned for after the task
/// is archived.
fn end_session(home: &Home, config: &Config, task: &Task) -> Result<Option<Window>> {
    let (session, window) = run::window_of(task, config);
    run::save_log(home, task, &session, &window);
    if std::env::var("FURMI_TASK_ID").is_ok_and(|v| v == task.id) {
        return Ok(Some((session, window)));
    }
    tmux::terminate_window(&session, &window, STOP_GRACE)?;
    Ok(None)
}

fn kill_deferred(window: Option<Window>) -> Result<()> {
    match window {
        Some((session, window)) => tmux::kill_window(&session, &window),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::TaskSpec;

    fn home() -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        for d in home.dirs() {
            std::fs::create_dir_all(d).unwrap();
        }
        (dir, home)
    }

    fn put(dir: PathBuf, id: &str, status: Status) {
        let spec = TaskSpec {
            repo: std::env::temp_dir(),
            branch: None,
            worktree: Some(false),
            agent: None,
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
        };
        let mut task = Task::new(id.into(), spec, "do it".into());
        task.status = status;
        task.error = Some("earlier failure".into());
        task.save(&dir.join(format!("{id}.md"))).unwrap();
    }

    fn status_of(home: &Home, id: &str) -> (Status, &'static str) {
        let (path, task) = find(home, id).unwrap();
        let place = if path.starts_with(home.archive()) {
            "archive"
        } else {
            "tasks"
        };
        (task.status, place)
    }

    #[test]
    fn cancel_then_retry_round_trip() {
        let (_d, home) = home();
        put(home.tasks(), "t1", Status::Pending);

        cancel(&home, Some("t1".into())).unwrap();
        assert_eq!(status_of(&home, "t1"), (Status::Cancelled, "archive"));
        assert!(cancel(&home, Some("t1".into())).is_err());

        retry(&home, "t1").unwrap();
        assert_eq!(status_of(&home, "t1"), (Status::Pending, "tasks"));
        let (_, t) = find(&home, "t1").unwrap();
        assert_eq!((t.finished_at, t.error), (None, None));
        assert!(!home.archive().join("t1.md").exists());
    }

    #[test]
    fn review_outcomes() {
        let (_d, home) = home();
        put(home.archive(), "ok", Status::NeedsReview);
        put(home.archive(), "bad", Status::NeedsReview);

        done(&home, Some("ok".into())).unwrap();
        let (_, t) = find(&home, "ok").unwrap();
        assert_eq!((t.status, t.error), (Status::Done, None));

        fail(&home, Some("bad".into()), Some("wrong approach".into())).unwrap();
        let (_, t) = find(&home, "bad").unwrap();
        assert_eq!(t.status, Status::Failed);
        assert_eq!(t.error.as_deref(), Some("wrong approach"));
    }

    #[test]
    fn logs_reports_missing_attempts_and_reads_saved_ones() {
        let (_d, home) = home();
        put(home.tasks(), "p", Status::Pending);
        assert!(logs(&home, "p", None).is_err());

        let (path, mut task) = find(&home, "p").unwrap();
        task.attempts = 2;
        task.status = Status::Failed;
        task.save(&path).unwrap();
        std::fs::write(home.logs().join("p.1.log"), "first\n").unwrap();
        std::fs::write(home.logs().join("p.2.log"), "second\n").unwrap();

        assert_eq!(logs(&home, "p", None).unwrap(), "second\n");
        assert_eq!(logs(&home, "p", Some(1)).unwrap(), "first\n");
        assert!(logs(&home, "p", Some(3)).is_err());
        assert!(logs(&home, "p", Some(0)).is_err());
    }

    #[test]
    fn rejects_wrong_status_and_unknown_ids() {
        let (_d, home) = home();
        put(home.tasks(), "p", Status::Pending);
        assert!(done(&home, Some("p".into())).is_err());
        assert!(fail(&home, Some("p".into()), None).is_err());
        assert!(retry(&home, "p").is_err());
        assert!(done(&home, Some("nope".into())).is_err());
        assert_eq!(status_of(&home, "p"), (Status::Pending, "tasks"));
    }
}
