//! Closing out an attempt: retries, `on_finish`, and the archive.

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset};

use crate::config::Config;
use crate::git;
use crate::home::{Home, contract_tilde};
use crate::spec::{Effective, OnFinish};
use crate::task::{Status, Task, now};

use super::log::log;
use super::reaping::{Outcome, note_in_log};
use super::starting::workdir;

/// Interruptions (see [`Outcome::Interrupted`]) a task absorbs before the
/// next one counts as a failure, so a task that keeps losing its window
/// can't loop forever.
const MAX_INTERRUPTIONS: u32 = 5;

/// Applies an outcome other than success, which `poll_running` hands to
/// [`finalize`] instead.
pub(super) fn finish(
    home: &Home,
    config: &Config,
    path: &Path,
    mut task: Task,
    outcome: Outcome,
    now: DateTime<FixedOffset>,
) -> Result<()> {
    let eff = task.spec.resolve(&config.defaults);
    match outcome {
        Outcome::Succeeded(_) => unreachable!("successes go through finalize"),
        Outcome::NeedsReview => {
            task.status = Status::NeedsReview;
            archive(home, path, task, now)
        }
        Outcome::Interrupted(reason) if task.interruptions < MAX_INTERRUPTIONS => {
            task.interruptions += 1;
            task.status = Status::Pending;
            task.scheduled_at = None;
            task.tmux_window = None;
            task.error = None;
            note_in_log(home, &task, &format!("attempt interrupted: {reason}"));
            log(&format!(
                "{} was interrupted (attempt {}): {reason}; requeueing without using a retry",
                task.id, task.attempts
            ));
            task.save(path)
        }
        Outcome::Interrupted(reason) => {
            let error = format!("{reason} (interrupted {MAX_INTERRUPTIONS} times)");
            note_in_log(home, &task, &format!("attempt interrupted: {reason}"));
            fail_attempt(home, path, task, &eff, error, true, now)
        }
        Outcome::Failed {
            error,
            exit_code,
            retry,
        } => {
            task.exit_code = exit_code;
            fail_attempt(home, path, task, &eff, error, retry, now)
        }
    }
}

/// Runs `on_finish` for a task that succeeded, then archives it as done (or
/// as failed if `on_finish` fails). Called without the home lock, so a slow
/// push can't block other commands; the lock is taken only to archive, and
/// only if the task is still as it was (not cancelled or finished by hand
/// meanwhile).
pub(super) fn finalize(home: &Home, config: &Config, path: &Path, mut task: Task) -> Result<()> {
    let result = run_on_finish(home, config, &task);
    let _lock = home.lock()?;
    match Task::load(path) {
        Ok(current) if current.status == task.status && current.attempts == task.attempts => {}
        _ => {
            log(&format!(
                "{} changed while on_finish ran; leaving it as it is",
                task.id
            ));
            return Ok(());
        }
    }
    record_finish(home, config, &mut task, result);
    archive(home, path, task, now())
}

/// Runs `on_finish` and archives the task as done, or as failed if
/// `on_finish` fails — the task is still archived (as failed, with the
/// details in its file) and an error is returned. Returns the final
/// status (`Done`).
///
/// `on_finish` runs without the home lock — a push or a PR can take
/// minutes and must not stall the runner or other commands — so the
/// lock is taken only to archive, and only if the task is still as it
/// was (not cancelled, retried or finished by hand meanwhile). The
/// runner's own equivalent is [`finalize`].
pub fn complete(
    home: &Home,
    config: &Config,
    path: &Path,
    task: Task,
    now: DateTime<FixedOffset>,
) -> Result<Status> {
    let result = run_on_finish(home, config, &task);
    let _lock = home.lock()?;
    match Task::load(path) {
        Ok(current) if current.status == task.status && current.attempts == task.attempts => {}
        Ok(current) => {
            log(&format!(
                "{} changed to {} while on_finish ran; leaving it as it is",
                task.id,
                current.status.as_str()
            ));
            bail!(
                "{} became {} while its on_finish ran, so it was not marked done",
                task.id,
                current.status.as_str()
            );
        }
        Err(e) => return Err(e).with_context(|| format!("re-reading {}", path.display())),
    }
    let mut task = task;
    record_finish(home, config, &mut task, result);
    let status = task.status;
    archive(home, path, task, now)?;
    if status != Status::Done {
        bail!("on_finish failed; the work is still in the task's worktree or repo");
    }
    Ok(status)
}

fn run_on_finish(home: &Home, config: &Config, task: &Task) -> Result<()> {
    let eff = task.spec.resolve(&config.defaults);
    let dir = workdir(home, task, &eff);
    if eff.on_finish != OnFinish::None {
        log_finish_changes(&task.id, &dir);
    }
    git::on_finish(eff.on_finish, &dir, &task.id, &task.prompt)
}

/// Says which paths `on_finish` is about to commit, before it commits
/// everything in `dir`. With `worktree: false` that folder is the user's own
/// checkout, so an edit swept into the task's commit can be recognised after
/// the fact. Purely informational: it never stops the commit.
fn log_finish_changes(id: &str, dir: &Path) {
    /// Paths named in the log; a build folder's leftovers can be thousands.
    const SHOWN: usize = 40;
    let paths = match git::changed_paths(dir) {
        Ok(paths) => paths,
        Err(e) => {
            log(&format!(
                "{id}: could not list the changes on_finish commits in {}: {e:#}",
                contract_tilde(dir)
            ));
            return;
        }
    };
    if paths.is_empty() {
        return;
    }
    let shown = paths.len().min(SHOWN);
    let mut msg = format!(
        "{id}: on_finish commits {} changed path(s) in {}:\n  {}",
        paths.len(),
        contract_tilde(dir),
        paths[..shown].join("\n  ")
    );
    if shown < paths.len() {
        msg.push_str(&format!("\n  ... and {} more", paths.len() - shown));
    }
    log(&msg);
}

fn record_finish(home: &Home, config: &Config, task: &mut Task, on_finish: Result<()>) {
    match on_finish {
        Ok(()) => {
            task.status = Status::Done;
            task.error = None;
        }
        Err(e) => {
            // `on_finish` commits before pushing or opening a PR, so a
            // failure here usually leaves the agent's work on the task's
            // branch: say where it is and how to pick it up again.
            let eff = task.spec.resolve(&config.defaults);
            let dir = workdir(home, task, &eff);
            let branch = task.effective_branch(config).unwrap_or_else(|| "-".into());
            task.status = Status::Failed;
            task.error = Some(format!(
                "on_finish failed: {e:#}\nThe worktree is kept at {} and the work is on branch `{branch}` \
                 (commit `on_finish` made, plus anything uncommitted). Fix the problem and run \
                 `furmi retry {id}` to finish it in the same worktree.",
                dir.display(),
                id = task.id
            ));
        }
    }
}

/// Requeues the task after `retry_delay` if a retry could help and it has
/// retries left, otherwise archives it as failed. `retry` is false for
/// failures a retry can't fix (e.g. the provider rejected the model).
pub(super) fn fail_attempt(
    home: &Home,
    path: &Path,
    mut task: Task,
    eff: &Effective,
    error: String,
    retry: bool,
    now: DateTime<FixedOffset>,
) -> Result<()> {
    task.error = Some(error);
    task.tmux_window = None;
    if retry
        && retry_left(
            task.attempts.saturating_sub(task.interruptions),
            eff.retries,
        )
    {
        task.status = Status::Pending;
        task.scheduled_at = Some(now + eff.retry_delay);
        log(&format!(
            "{} failed (attempt {}): {}; retrying in {}",
            task.id,
            task.attempts,
            task.error.as_deref().unwrap_or_default(),
            humantime::format_duration(eff.retry_delay)
        ));
        task.save(path)
    } else {
        task.status = Status::Failed;
        archive(home, path, task, now)
    }
}

/// `retries` counts extra attempts after the first.
fn retry_left(attempts: u32, retries: u32) -> bool {
    attempts <= retries
}

/// Stamps `finished_at` and moves the task to `archive/` (a no-op move if
/// it's already there).
pub fn archive(home: &Home, path: &Path, mut task: Task, now: DateTime<FixedOffset>) -> Result<()> {
    task.finished_at = Some(now);
    task.save(path)?;
    let dest = home.archive().join(format!("{}.md", task.id));
    std::fs::rename(path, &dest)
        .with_context(|| format!("moving {} to {}", path.display(), dest.display()))?;
    let detail = task
        .error
        .as_deref()
        .map(|e| format!(": {e}"))
        .unwrap_or_default();
    log(&format!("{} {}{detail}", task.id, task.status.as_str()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_count_extra_attempts() {
        assert!(retry_left(1, 1));
        assert!(!retry_left(2, 1));
        assert!(!retry_left(1, 0));
    }
}
