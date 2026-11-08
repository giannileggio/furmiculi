//! Housekeeping: the runner tidying up after itself, at start and then every
//! `[housekeeping] interval`.
//!
//! Three kinds of thing happen here, and the line between them is the point:
//!
//! - **Safe actions**, done automatically and logged: removing the worktree
//!   of a *done* task that git can inspect and that has no uncommitted
//!   changes; trimming old attempts' log files; requeueing a failed task
//!   whose cause was the machine (tmux gone, agent not installed) once that
//!   cause is gone, a capped number of times.
//! - **Health**, written to `health.json` for the dashboard: the
//!   `furmi doctor` checks that aren't ok, anything the pass declined to do
//!   and why, and tasks whose worktree stayed clean while the base checkout
//!   gained changes (see [`super::escape`]).
//! - **Suggestions**, destructive things (a merged `furmi/*` branch) that are
//!   only listed, each with the exact command to run. Never done here.
//!
//! A worktree is removed only when git can inspect it, it is its own
//! checkout, and it reports no changes. If any of that is in doubt it stays,
//! and the health list says so.

use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::cmd::md_files;
use crate::config::{Config, on_path};
use crate::fsutil;
use crate::git;
use crate::home::Home;
use crate::task::{Status, Task, now};
use crate::{control, doctor, tmux};

use super::escape;
use super::log::log;
use super::starting::start_problems;

/// Most suggestions listed individually; the rest are summarised.
const MAX_SUGGESTIONS: usize = 20;

/// One line of the dashboard's health list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    /// `warn`, `fail` (from `furmi doctor`) or `info` (a suggestion).
    pub level: String,
    pub message: String,
    /// The exact command to run, for a suggestion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// What the dashboard's button does instead of that command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
}

impl Health {
    fn new(level: &str, message: impl Into<String>) -> Self {
        Self {
            level: level.into(),
            message: message.into(),
            command: None,
            fix: None,
        }
    }
}

/// An action the dashboard can run for a health item. Only things that are safe
/// to repeat and can't lose work: a retry, and `git branch -d` of a merged branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fix {
    Retry { id: String },
    DeleteBranch { repo: PathBuf, branch: String },
}

impl Fix {
    fn apply(&self, home: &Home) -> anyhow::Result<()> {
        match self {
            Fix::Retry { id } => control::retry(home, id),
            Fix::DeleteBranch { repo, branch } => {
                // Checked again now: the branch may have moved on since the pass.
                let base = git::main_branch(repo)
                    .ok_or_else(|| anyhow::anyhow!("{} has no main branch", repo.display()))?;
                if !branch.starts_with("furmi/") || !git::branch_merged_into(repo, branch, &base) {
                    anyhow::bail!("{branch} is no longer a merged furmi/* branch");
                }
                git::delete_merged_branch(repo, branch)
            }
        }
    }
}

/// Runs the fix of one health item (`only`), or of every item that has one
/// (`None`). A fix is run only if the last pass offered it, so the endpoint
/// can't be used to delete an arbitrary branch. Fixed items leave the list at
/// once; the next pass rebuilds it. Returns how many were fixed, or the errors.
pub fn apply_fixes(home: &Home, only: Option<&Fix>) -> Result<usize, Vec<String>> {
    let mut health = load_health(home);
    let mut errors = Vec::new();
    let mut fixed = 0;
    health.retain(|h| {
        let Some(fix) = &h.fix else { return true };
        if only.is_some_and(|o| o != fix) {
            return true;
        }
        match fix.apply(home) {
            Ok(()) => {
                log(&format!("housekeeping: fixed from the dashboard: {fix:?}"));
                fixed += 1;
                false
            }
            Err(e) => {
                errors.push(format!("{e:#}"));
                true
            }
        }
    });
    if only.is_some() && fixed == 0 && errors.is_empty() {
        errors.push("that is no longer on the list".into());
    }
    let _ = fsutil::write_atomic(
        &home.health_file(),
        serde_json::to_string(&health).unwrap_or_else(|_| "[]".into()),
    );
    if errors.is_empty() {
        Ok(fixed)
    } else {
        Err(errors)
    }
}

/// Reads the last pass's health list (empty if there is none).
pub fn load_health(home: &Home) -> Vec<Health> {
    std::fs::read_to_string(home.health_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Decides when the pass is due. The runner calls [`Housekeeper::tick`] once
/// per loop iteration.
pub(super) struct Housekeeper {
    last: Option<Instant>,
}

impl Housekeeper {
    pub(super) fn new() -> Self {
        Self { last: None }
    }

    /// Runs the pass if it is due. `mutate` is false while the runner is
    /// paused: health is still reported, but nothing is changed.
    pub(super) fn tick(&mut self, home: &Home, config: &Config, mutate: bool) {
        let hk = &config.housekeeping;
        if !hk.enabled {
            self.last = None;
            let _ = std::fs::remove_file(home.health_file());
            return;
        }
        if self.last.is_some_and(|t| t.elapsed() < hk.interval) {
            return;
        }
        self.last = Some(Instant::now());
        let health = pass(home, config, mutate);
        if let Err(e) = fsutil::write_atomic(
            &home.health_file(),
            serde_json::to_string(&health).unwrap_or_else(|_| "[]".into()),
        ) {
            log(&format!(
                "housekeeping: could not write the health list: {e:#}"
            ));
        }
    }
}

/// One pass. Every action is logged; the returned list is the health report.
pub fn pass(home: &Home, config: &Config, mutate: bool) -> Vec<Health> {
    let mut health: Vec<Health> = doctor::checks(home)
        .iter()
        .filter(|c| c.level() != "ok")
        .map(|c| Health::new(c.level(), c.message()))
        .collect();
    let now = now();
    health.extend(escape::health(home, mutate));
    if mutate {
        let hk = &config.housekeeping;
        health.extend(requeue_environmental(home, config, hk.max_requeues));
        health.extend(remove_clean_worktrees(home, now));
        trim_logs(home, hk.keep_attempts);
    }
    health.extend(suggest_branch_deletions(home, config));
    health
}

// --- worktrees -------------------------------------------------------------------

/// Removes `worktrees/<id>` of done tasks whose worktree git can inspect and
/// finds clean. Returns health items for the ones it had to leave alone.
/// Uses the same eligibility rule as `furmi clean` (done only) and the same
/// `git worktree remove`, which itself refuses a dirty tree.
fn remove_clean_worktrees(home: &Home, now: DateTime<FixedOffset>) -> Vec<Health> {
    let mut health = Vec::new();
    for path in md_files(&home.archive()).unwrap_or_default() {
        let Ok(task) = Task::load(&path) else {
            continue;
        };
        if crate::clean::eligible(task.status, task.finished_at, false, None, now).is_err() {
            continue;
        }
        let dir = home.worktrees().join(&task.id);
        if !dir.is_dir() {
            continue;
        }
        // Keeps `furmi run`/`furmi retry` from using a worktree mid-removal.
        let Ok(_lock) = home.lock() else {
            continue;
        };
        // Retried since the archive was read: it's in use again.
        if home.tasks().join(format!("{}.md", task.id)).exists() {
            continue;
        }
        match inspect(&dir) {
            Inspected::Clean => match git::remove_worktree(&dir) {
                Ok(()) => log(&format!(
                    "housekeeping: removed the worktree of done task {} ({})",
                    task.id,
                    dir.display()
                )),
                Err(e) => health.push(Health::new(
                    "warn",
                    format!("could not remove the worktree of {}: {e:#}", task.id),
                )),
            },
            Inspected::Dirty => health.push(Health::new(
                "warn",
                format!(
                    "{} is done but its worktree has uncommitted changes, so it was kept: {}",
                    task.id,
                    dir.display()
                ),
            )),
            Inspected::Unknown(why) => health.push(Health::new(
                "warn",
                format!(
                    "git can't inspect the worktree of {} ({why}), so it was kept; \
                     look at {} and remove it by hand if it is not needed",
                    task.id,
                    dir.display()
                ),
            )),
        }
    }
    health
}

enum Inspected {
    Clean,
    Dirty,
    /// Git couldn't say, or the directory isn't a checkout of its own.
    /// Treated like dirty: never removed.
    Unknown(String),
}

/// Whether `dir` is a checkout git can inspect, and whether it has changes.
/// Stricter than `furmi clean`: anything short of a positive "clean" is
/// "don't touch". A plain folder inside some other repo would make
/// `git status` describe *that* repo, so the folder must be its own root.
fn inspect(dir: &Path) -> Inspected {
    if !dir.join(".git").exists() {
        return Inspected::Unknown("it has no .git".into());
    }
    match git::repo_root(dir) {
        Ok(root) if same_path(&root, dir) => {}
        Ok(root) => {
            return Inspected::Unknown(format!("it is inside the checkout at {}", root.display()));
        }
        Err(e) => return Inspected::Unknown(format!("{e:#}")),
    }
    match git::has_changes(dir) {
        Ok(false) => Inspected::Clean,
        Ok(true) => Inspected::Dirty,
        Err(e) => Inspected::Unknown(format!("{e:#}")),
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// --- logs ------------------------------------------------------------------------

/// Deletes the `.log`/`.exit`/`.usage` files of all but the newest
/// `keep` attempts of each task.
fn trim_logs(home: &Home, keep: u32) {
    let Ok(entries) = std::fs::read_dir(home.logs()) else {
        return;
    };
    let mut by_task: std::collections::BTreeMap<String, Vec<(u32, PathBuf)>> = Default::default();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let mut parts = name.rsplitn(3, '.');
        let (Some(ext), Some(attempt), Some(id)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !matches!(ext, "log" | "exit" | "usage" | "base" | "escaped") {
            continue;
        }
        if let Ok(attempt) = attempt.parse::<u32>() {
            by_task
                .entry(id.to_string())
                .or_default()
                .push((attempt, entry.path()));
        }
    }
    for (id, files) in by_task {
        let mut attempts: Vec<u32> = files.iter().map(|(a, _)| *a).collect();
        attempts.sort_unstable();
        attempts.dedup();
        let Some(&cutoff) = attempts.iter().rev().nth(keep as usize - 1) else {
            continue;
        };
        let mut removed = 0;
        for (attempt, path) in &files {
            if *attempt < cutoff && std::fs::remove_file(path).is_ok() {
                removed += 1;
            }
        }
        if removed > 0 {
            log(&format!(
                "housekeeping: trimmed {removed} log file(s) of {id} older than its last {keep} attempts"
            ));
        }
    }
}

// --- environmental failures ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    /// The agent's program wasn't on `$PATH`.
    AgentMissing,
    /// tmux was missing or unusable.
    Tmux,
}

/// Why a failed task failed, if the cause was the machine and not the task.
fn classify(task: &Task) -> Option<Cause> {
    let error = task.error.as_deref()?;
    if error.starts_with("invalid task:") && error.contains("is not on $PATH") {
        return Some(Cause::AgentMissing);
    }
    if task.exit_code == Some(127) {
        return Some(Cause::AgentMissing);
    }
    let from_tmux = error.starts_with("could not start:") && error.contains("tmux");
    let lost_window = error.contains("(interrupted ") && error.contains(" times)");
    (from_tmux || lost_window).then_some(Cause::Tmux)
}

/// Whether the cause no longer applies, so a retry has a chance.
fn cleared(cause: Cause, home: &Home, config: &Config, task: &Task) -> bool {
    match cause {
        Cause::AgentMissing => start_problems(
            config,
            task,
            &home.archive().join(format!("{}.md", task.id)),
        )
        .is_empty(),
        Cause::Tmux => {
            on_path("tmux") && tmux::session_exists(&config.runner.tmux_session).unwrap_or(false)
        }
    }
}

/// Requeues failed tasks whose environmental cause has cleared, at most
/// `max` times each. A task that hit the cap is reported, not retried.
fn requeue_environmental(home: &Home, config: &Config, max: u32) -> Vec<Health> {
    let mut health = Vec::new();
    for path in md_files(&home.archive()).unwrap_or_default() {
        let Ok(task) = Task::load(&path) else {
            continue;
        };
        if task.status != Status::Failed {
            continue;
        }
        let Some(cause) = classify(&task) else {
            continue;
        };
        if task.auto_requeues >= max {
            health.push(Health {
                level: "warn".into(),
                message: format!(
                    "{} failed for an environmental reason ({cause:?}) and was already requeued \
                     {} time(s); fix the cause, then retry it",
                    task.id, task.auto_requeues
                ),
                command: Some(format!("furmi retry {}", task.id)),
                fix: Some(Fix::Retry {
                    id: task.id.clone(),
                }),
            });
            continue;
        }
        if !cleared(cause, home, config, &task) {
            continue;
        }
        // Keeps this from racing `furmi retry` or the runner over the task file.
        let Ok(_lock) = home.lock() else {
            continue;
        };
        // Re-read under the lock: it may have been retried or removed since.
        let Ok(mut task) = Task::load(&path) else {
            continue;
        };
        if task.status != Status::Failed || task.auto_requeues >= max {
            continue;
        }
        // Count first, so a crash can't make the cap unreachable.
        task.auto_requeues += 1;
        let requeues = task.auto_requeues;
        if let Err(e) = task.save(&path) {
            log(&format!(
                "housekeeping: could not update {}: {e:#}",
                task.id
            ));
            continue;
        }
        match control::retry_locked(home, &task.id) {
            Ok(()) => log(&format!(
                "housekeeping: requeued {} ({cause:?} is fixed; automatic requeue {requeues}/{max})",
                task.id
            )),
            Err(e) => {
                log(&format!(
                    "housekeeping: could not requeue {}: {e:#}",
                    task.id
                ));
                task.auto_requeues -= 1;
                let _ = task.save(&path);
            }
        }
    }
    health
}

// --- suggestions -----------------------------------------------------------------

/// Lists `furmi/*` branches of archived tasks that are merged into the repo's
/// main branch and no longer checked out. Only listed, with the command.
fn suggest_branch_deletions(home: &Home, config: &Config) -> Vec<Health> {
    let mut found: Vec<(PathBuf, String, String)> = Vec::new();
    for path in md_files(&home.archive()).unwrap_or_default() {
        let Ok(task) = Task::load(&path) else {
            continue;
        };
        if task.status != Status::Done || home.worktrees().join(&task.id).exists() {
            continue;
        }
        let Some(branch) = task
            .effective_branch(config)
            .filter(|b| b.starts_with("furmi/"))
        else {
            continue;
        };
        let repo = task.spec.resolve(&config.defaults).repo;
        let Some(base) = git::main_branch(&repo) else {
            continue;
        };
        if branch != base
            && git::branch_merged_into(&repo, &branch, &base)
            && !found.iter().any(|(r, b, _)| *r == repo && *b == branch)
        {
            found.push((repo, branch, base));
        }
    }
    let mut out: Vec<Health> = found
        .iter()
        .take(MAX_SUGGESTIONS)
        .map(|(repo, branch, base)| Health {
            level: "info".into(),
            message: format!("branch {branch} is merged into {base} and its task is archived"),
            command: Some(format!(
                "git -C {} branch -d {}",
                shell_quote(&repo.to_string_lossy()),
                shell_quote(branch)
            )),
            fix: Some(Fix::DeleteBranch {
                repo: repo.clone(),
                branch: branch.clone(),
            }),
        })
        .collect();
    if found.len() > MAX_SUGGESTIONS {
        out.push(Health::new(
            "info",
            format!(
                "...and {} more merged furmi/* branches",
                found.len() - MAX_SUGGESTIONS
            ),
        ));
    }
    out
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

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

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "T"],
            &["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            git_in(dir.path(), args);
        }
        dir
    }

    fn spec(repo: &Path) -> TaskSpec {
        TaskSpec {
            repo: repo.to_path_buf(),
            branch: None,
            worktree: Some(true),
            agent: None,
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
            run: None,
        }
    }

    fn archive(
        home: &Home,
        repo: &Path,
        id: &str,
        status: Status,
        edit: impl FnOnce(&mut Task),
    ) -> Task {
        let mut task = Task::new(id.into(), spec(repo), "do it".into());
        task.status = status;
        task.finished_at = Some(now());
        edit(&mut task);
        task.save(&home.archive().join(format!("{id}.md"))).unwrap();
        task
    }

    /// A worktree for `id` the way the runner makes one.
    fn worktree(home: &Home, repo: &Path, id: &str) -> PathBuf {
        let dir = home.worktrees().join(id);
        git_in(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("furmi/{id}"),
                dir.to_str().unwrap(),
            ],
        );
        dir
    }

    fn items_mention(items: &[Health], needle: &str) -> bool {
        items.iter().any(|h| h.message.contains(needle))
    }

    #[test]
    fn removes_the_clean_worktree_of_a_done_task() {
        let (_d, home) = home();
        let repo = repo();
        let dir = worktree(&home, repo.path(), "t1");
        archive(&home, repo.path(), "t1", Status::Done, |_| {});

        let health = remove_clean_worktrees(&home, now());

        assert!(!dir.exists());
        assert!(health.is_empty());
    }

    #[test]
    fn never_removes_a_dirty_worktree() {
        let (_d, home) = home();
        let repo = repo();
        let dir = worktree(&home, repo.path(), "t1");
        std::fs::write(dir.join("wip.txt"), "unsaved work").unwrap();
        archive(&home, repo.path(), "t1", Status::Done, |_| {});

        let health = remove_clean_worktrees(&home, now());

        assert!(dir.join("wip.txt").exists());
        assert!(items_mention(&health, "uncommitted changes"));
    }

    #[test]
    fn never_removes_a_worktree_git_cannot_inspect() {
        let (_d, home) = home();
        let repo = repo();
        let dir = worktree(&home, repo.path(), "t1");
        archive(&home, repo.path(), "t1", Status::Done, |_| {});
        // The owning repo goes away: every git command in the worktree fails.
        drop(repo);

        let health = remove_clean_worktrees(&home, now());

        assert!(dir.join(".git").exists(), "the worktree must stay");
        assert!(items_mention(&health, "can't inspect"));
    }

    #[test]
    fn never_removes_a_plain_folder_that_sits_inside_another_checkout() {
        let (_d, home) = home();
        // The home itself lives in a repo: `git status` in a plain
        // worktrees/<id> folder would describe that repo, and say "clean".
        git_in(home.root(), &["init", "-q", "-b", "main"]);
        let dir = home.worktrees().join("t1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("precious.txt"), "x").unwrap();
        let repo = repo();
        archive(&home, repo.path(), "t1", Status::Done, |_| {});

        let health = remove_clean_worktrees(&home, now());

        assert!(dir.join("precious.txt").exists());
        assert!(items_mention(&health, "can't inspect"));
    }

    #[test]
    fn leaves_the_worktrees_of_tasks_that_are_not_done() {
        let (_d, home) = home();
        let repo = repo();
        let dir = worktree(&home, repo.path(), "t1");
        archive(&home, repo.path(), "t1", Status::Failed, |_| {});

        remove_clean_worktrees(&home, now());

        assert!(dir.exists());
    }

    #[test]
    fn trims_logs_past_the_newest_attempts_of_each_task() {
        let (_d, home) = home();
        for attempt in 1..=7 {
            for ext in ["log", "exit", "usage"] {
                std::fs::write(home.logs().join(format!("a.{attempt}.{ext}")), "x").unwrap();
            }
        }
        std::fs::write(home.logs().join("b.1.log"), "x").unwrap();

        trim_logs(&home, 5);

        for attempt in 1..=2 {
            assert!(!home.logs().join(format!("a.{attempt}.log")).exists());
            assert!(!home.logs().join(format!("a.{attempt}.exit")).exists());
            assert!(!home.logs().join(format!("a.{attempt}.usage")).exists());
        }
        for attempt in 3..=7 {
            assert!(home.logs().join(format!("a.{attempt}.log")).exists());
        }
        assert!(home.logs().join("b.1.log").exists());
    }

    #[test]
    fn classifies_environmental_failures_only() {
        let task = |error: &str, code: Option<i32>| {
            let mut t = Task::new("t".into(), spec(Path::new("/tmp")), "p".into());
            t.error = Some(error.into());
            t.exit_code = code;
            t
        };
        let missing = "invalid task: agent `x`: `x` is not on $PATH; install it";
        assert_eq!(classify(&task(missing, None)), Some(Cause::AgentMissing));
        assert_eq!(
            classify(&task("exited with status 127", Some(127))),
            Some(Cause::AgentMissing)
        );
        assert_eq!(
            classify(&task("could not start: tmux new-window failed", None)),
            Some(Cause::Tmux)
        );
        assert_eq!(
            classify(&task("tmux window disappeared (interrupted 5 times)", None)),
            Some(Cause::Tmux)
        );
        assert_eq!(classify(&task("exited with status 1", Some(1))), None);
        assert_eq!(classify(&task("timed out after 2h", None)), None);
        assert_eq!(
            classify(&task("could not start: branch already exists", None)),
            None
        );
    }

    #[test]
    fn requeues_once_the_agent_is_installed_and_stops_at_the_cap() {
        let (_d, home) = home();
        let repo = repo();
        let agent = home.root().join("fake-agent");
        let config = Config::parse(&format!(
            "[agents.fake]\nheadless = [\"{0}\"]\ninteractive = [\"{0}\"]\n",
            agent.display()
        ))
        .unwrap();
        let fail = |home: &Home| {
            archive(home, repo.path(), "t1", Status::Failed, |t| {
                t.spec.agent = Some("fake".into());
                t.error = Some(format!(
                    "invalid task: agent `fake`: `{}` is not on $PATH",
                    agent.display()
                ));
            })
        };
        fail(&home);

        // Still not installed: nothing happens, and no requeue is spent.
        requeue_environmental(&home, &config, 2);
        assert!(home.archive().join("t1.md").exists());
        assert_eq!(
            Task::load(&home.archive().join("t1.md"))
                .unwrap()
                .auto_requeues,
            0
        );

        // Installed: requeued, with the count recorded.
        std::fs::write(&agent, "#!/bin/sh\n").unwrap();
        requeue_environmental(&home, &config, 2);
        let queued = Task::load(&home.tasks().join("t1.md")).unwrap();
        assert_eq!(queued.status, Status::Pending);
        assert_eq!(queued.auto_requeues, 1);

        // It fails the same way again with the cap already spent: it stays
        // failed, and the health list says how to retry it by hand.
        std::fs::remove_file(home.tasks().join("t1.md")).unwrap();
        let mut t = fail(&home);
        t.auto_requeues = 2;
        t.save(&home.archive().join("t1.md")).unwrap();
        let health = requeue_environmental(&home, &config, 2);
        assert!(home.archive().join("t1.md").exists());
        assert!(!home.tasks().join("t1.md").exists());
        assert_eq!(health[0].command.as_deref(), Some("furmi retry t1"));
    }

    #[test]
    fn a_missing_tmux_session_does_not_count_as_cleared() {
        let (_d, home) = home();
        let mut config = Config::default();
        config.runner.tmux_session = format!("furmi-no-such-session-{}", std::process::id());
        let task = Task::new("t".into(), spec(Path::new("/tmp")), "p".into());

        assert!(!cleared(Cause::Tmux, &home, &config, &task));
    }

    #[test]
    fn does_not_requeue_a_task_that_failed_on_its_own() {
        let (_d, home) = home();
        let repo = repo();
        archive(&home, repo.path(), "t1", Status::Failed, |t| {
            t.error = Some("exited with status 1".into());
            t.exit_code = Some(1);
        });

        requeue_environmental(&home, &Config::default(), 2);

        assert!(home.archive().join("t1.md").exists());
        assert!(!home.tasks().join("t1.md").exists());
    }

    #[test]
    fn fixing_deletes_only_what_the_last_pass_offered() {
        let (_d, home) = home();
        let repo = repo();
        git_in(repo.path(), &["branch", "furmi/t1"]);
        git_in(repo.path(), &["branch", "furmi/other"]);
        archive(&home, repo.path(), "t1", Status::Done, |_| {});
        let health = suggest_branch_deletions(&home, &Config::default());
        std::fs::write(home.health_file(), serde_json::to_string(&health).unwrap()).unwrap();

        // A branch nobody offered is refused.
        let stray = Fix::DeleteBranch {
            repo: repo.path().to_path_buf(),
            branch: "furmi/other".into(),
        };
        assert!(apply_fixes(&home, Some(&stray)).is_err());

        assert_eq!(apply_fixes(&home, None), Ok(1));
        let left = Command::new("git")
            .args(["branch", "--list", "furmi/*"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let left = String::from_utf8_lossy(&left.stdout);
        assert!(!left.contains("furmi/t1") && left.contains("furmi/other"));
        assert!(load_health(&home).is_empty());
    }

    #[test]
    fn suggests_deleting_a_merged_branch_but_leaves_it_alone() {
        let (_d, home) = home();
        let repo = repo();
        // t1's branch is merged (same commit as main); t2's has its own commit.
        git_in(repo.path(), &["branch", "furmi/t1"]);
        git_in(repo.path(), &["checkout", "-q", "-b", "furmi/t2"]);
        git_in(
            repo.path(),
            &["commit", "-q", "--allow-empty", "-m", "work"],
        );
        git_in(repo.path(), &["checkout", "-q", "main"]);
        archive(&home, repo.path(), "t1", Status::Done, |_| {});
        archive(&home, repo.path(), "t2", Status::Done, |_| {});

        let health = suggest_branch_deletions(&home, &Config::default());

        assert_eq!(health.len(), 1);
        assert_eq!(health[0].level, "info");
        assert!(health[0].message.contains("furmi/t1"));
        let command = health[0].command.as_deref().unwrap();
        assert!(command.starts_with("git -C "));
        assert!(command.ends_with("branch -d furmi/t1"));
        // Only suggested.
        let branches = Command::new("git")
            .args(["branch", "--list", "furmi/*"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&branches.stdout);
        assert!(branches.contains("furmi/t1") && branches.contains("furmi/t2"));
    }

    #[test]
    fn a_paused_pass_changes_nothing() {
        let (_d, home) = home();
        let repo = repo();
        let dir = worktree(&home, repo.path(), "t1");
        archive(&home, repo.path(), "t1", Status::Done, |_| {});
        for attempt in 1..=7 {
            std::fs::write(home.logs().join(format!("t1.{attempt}.log")), "x").unwrap();
        }

        pass(&home, &Config::default(), false);

        assert!(dir.exists());
        assert!(home.logs().join("t1.1.log").exists());
    }

    #[test]
    fn the_pass_writes_health_and_the_off_switch_clears_it() {
        let (_d, home) = home();
        let mut config = Config::default();
        let mut hk = Housekeeper::new();

        hk.tick(&home, &config, true);
        assert!(home.health_file().exists());
        // A second tick inside the interval does nothing.
        std::fs::remove_file(home.health_file()).unwrap();
        hk.tick(&home, &config, true);
        assert!(!home.health_file().exists());

        config.housekeeping.enabled = false;
        std::fs::write(home.health_file(), "[]").unwrap();
        hk.tick(&home, &config, true);
        assert!(!home.health_file().exists());
    }

    #[test]
    fn quotes_paths_for_the_shell() {
        assert_eq!(shell_quote("/home/a/repo"), "/home/a/repo");
        assert_eq!(shell_quote("/home/a b/it's"), r"'/home/a b/it'\''s'");
    }
}
