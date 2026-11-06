//! Catching an agent that edits the shared checkout instead of its worktree.
//!
//! A worktree task is meant to work in `worktrees/<id>`. An agent given the
//! repo's absolute path in its prompt can `cd` to the main checkout instead,
//! leaving its own worktree clean. `on_finish` then finds nothing to commit
//! and treats the run as a legitimate no-op (see `git::on_finish`), so the
//! task looks done with no commit or PR while its edits sit in the shared
//! checkout. This notices that: the base checkout's dirty paths are saved
//! when an attempt starts and compared when it ends.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::git;
use crate::home::{Home, contract_tilde};
use crate::spec::{Effective, OnFinish};
use crate::task::Task;

use super::housekeeping::Health;
use super::log::log;
use super::reaping::note_in_log;
use super::starting::workdir;

/// Paths named in a log note; a build folder's leftovers can be thousands.
const SHOWN: usize = 40;
/// Paths named in a health warning, which has to fit the dashboard's banner.
const SHOWN_IN_HEALTH: usize = 8;

/// What [`report`] leaves for housekeeping to turn into a health warning.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    repo: PathBuf,
    paths: Vec<String>,
}

fn base_file(home: &Home, id: &str, attempt: u32) -> PathBuf {
    home.logs().join(format!("{id}.{attempt}.base"))
}

fn record_file(home: &Home, id: &str, attempt: u32) -> PathBuf {
    home.logs().join(format!("{id}.{attempt}.escaped"))
}

/// Whether this attempt is one the check applies to: a worktree task with an
/// `on_finish` step that could have committed the work.
fn tracked(eff: &Effective) -> bool {
    eff.worktree && eff.on_finish != OnFinish::None
}

/// Saves the base checkout's dirty paths before the attempt starts. A failure
/// only turns detection off for this attempt; it never stops the task.
pub(super) fn snapshot(home: &Home, task: &Task, eff: &Effective) {
    if !tracked(eff) {
        return;
    }
    let paths = match git::changed_paths(&eff.repo) {
        Ok(paths) => paths,
        Err(e) => {
            log(&format!(
                "{}: could not read the base checkout's changes, so an edit there won't be \
                 recognised: {e:#}",
                task.id
            ));
            return;
        }
    };
    let path = base_file(home, &task.id, task.attempts);
    if let Err(e) = fsutil::write_atomic(&path, paths.join("\n")) {
        log(&format!(
            "{}: could not save the base checkout's state, so an edit there won't be \
             recognised: {e:#}",
            task.id
        ));
    }
}

/// The base-checkout paths this attempt added, if its worktree is still clean.
/// `None` when the task didn't run in a worktree, has no `on_finish`, the
/// worktree has changes (the agent worked where it should), the snapshot is
/// missing, or git can't answer. A false negative is preferable to accusing a
/// task that did real work in its own worktree.
pub(super) fn escaped_paths(home: &Home, task: &Task, eff: &Effective) -> Option<Vec<String>> {
    if !tracked(eff) {
        return None;
    }
    let dir = workdir(home, task, eff);
    if !matches!(git::has_changes(&dir), Ok(false)) {
        return None;
    }
    let before = std::fs::read_to_string(base_file(home, &task.id, task.attempts)).ok()?;
    let before: Vec<&str> = before
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let now = git::changed_paths(&eff.repo).ok()?;
    let added: Vec<String> = now
        .into_iter()
        .filter(|path| !before.contains(&path.as_str()))
        .collect();
    (!added.is_empty()).then_some(added)
}

/// Makes an escaped edit visible: a note in the attempt's log naming the
/// changed paths, and a marker housekeeping turns into a health warning.
pub(super) fn report(home: &Home, task: &Task, eff: &Effective, paths: &[String]) {
    let dir = workdir(home, task, eff);
    let shown = paths.len().min(SHOWN);
    let mut note = format!(
        "the worktree {} is clean, but the base checkout {} gained {} change(s) during this \
         attempt:\n  {}",
        contract_tilde(&dir),
        contract_tilde(&eff.repo),
        paths.len(),
        paths[..shown].join("\n  ")
    );
    if shown < paths.len() {
        note.push_str(&format!("\n  ... and {} more", paths.len() - shown));
    }
    note.push_str(
        "\nThis usually means the agent edited the shared checkout instead of its worktree, so \
         on_finish found nothing to commit and the task looks like a no-op. The changes are in \
         the base checkout, not in the task's branch.",
    );
    note_in_log(home, task, &note);
    log(&format!(
        "{}: warning: its worktree is clean but the base checkout {} changed during the attempt; \
         the agent may have edited the shared checkout instead (see its log)",
        task.id,
        contract_tilde(&eff.repo)
    ));

    let record = Record {
        repo: eff.repo.clone(),
        paths: paths.to_vec(),
    };
    let result = serde_json::to_string(&record)
        .map_err(anyhow::Error::from)
        .and_then(|json| fsutil::write_atomic(&record_file(home, &task.id, task.attempts), json));
    if let Err(e) = result {
        log(&format!(
            "{}: could not record the escaped checkout warning: {e:#}",
            task.id
        ));
    }
}

/// Health warnings for escaped edits whose changes are still in the base
/// checkout. A marker whose paths are all gone (committed, stashed or
/// reverted) is removed, so the warning clears once the shared checkout does.
/// `prune` is false while the runner is paused, when housekeeping changes
/// nothing.
pub(super) fn health(home: &Home, prune: bool) -> Vec<Health> {
    let Ok(entries) = std::fs::read_dir(home.logs()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".escaped") else {
            continue;
        };
        // `<id>.<attempt>.escaped`; task ids can't contain a dot.
        let Some((id, _attempt)) = stem.rsplit_once('.') else {
            continue;
        };
        let marker = entry.path();
        let Ok(text) = std::fs::read_to_string(&marker) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Record>(&text) else {
            continue;
        };
        if !still_changed(&record) {
            if prune {
                let _ = std::fs::remove_file(&marker);
            }
            continue;
        }
        let shown = record.paths.len().min(SHOWN_IN_HEALTH);
        let mut paths = record.paths[..shown].join(", ");
        if shown < record.paths.len() {
            paths.push_str(&format!(", and {} more", record.paths.len() - shown));
        }
        out.push(Health {
            level: "warn".into(),
            message: format!(
                "{id} is done but its worktree is clean while the base checkout {} gained \
                 changes during the attempt: {paths}. The agent may have edited the shared \
                 checkout instead of its worktree.",
                contract_tilde(&record.repo)
            ),
            command: None,
        });
    }
    out.sort_by(|a, b| a.message.cmp(&b.message));
    out
}

/// Whether any path from the marker is still changed in the base checkout.
/// Git failing counts as still there: never clear a warning on a guess.
fn still_changed(record: &Record) -> bool {
    match git::changed_paths(&record.repo) {
        Ok(now) => record.paths.iter().any(|path| now.contains(path)),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::config::Config;
    use crate::spec::TaskSpec;
    use crate::task::Status;

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

    fn task(repo: &Path, id: &str) -> (Task, Effective) {
        let spec = TaskSpec {
            repo: repo.to_path_buf(),
            branch: None,
            worktree: Some(true),
            agent: None,
            model: None,
            mode: None,
            on_finish: Some(OnFinish::Commit),
            retries: None,
            retry_delay: None,
            timeout: None,
            run: None,
        };
        let mut task = Task::new(id.into(), spec, "do it".into());
        task.status = Status::Running;
        task.attempts = 1;
        let eff = task.spec.resolve(&Config::default().defaults);
        (task, eff)
    }

    fn log_of(home: &Home, id: &str) -> String {
        std::fs::read_to_string(home.logs().join(format!("{id}.1.log"))).unwrap_or_default()
    }

    #[test]
    fn an_untouched_worktree_with_a_changed_base_checkout_is_reported() {
        let (_d, home) = home();
        let repo = repo();
        let id = "t1";
        worktree(&home, repo.path(), id);
        let (task, eff) = task(repo.path(), id);
        snapshot(&home, &task, &eff);

        // The agent does its work in the shared checkout instead.
        std::fs::write(repo.path().join("escaped.txt"), "x").unwrap();

        let paths = escaped_paths(&home, &task, &eff).expect("the escape is noticed");
        assert!(paths.iter().any(|p| p.contains("escaped.txt")), "{paths:?}");
        report(&home, &task, &eff, &paths);

        let log = log_of(&home, id);
        assert!(log.contains("base checkout"), "{log}");
        assert!(log.contains("escaped.txt"), "{log}");

        let health = health(&home, true);
        assert_eq!(health.len(), 1, "{health:?}");
        assert_eq!(health[0].level, "warn");
        assert!(health[0].message.contains("escaped.txt"), "{health:?}");
    }

    #[test]
    fn a_legitimate_noop_stays_quiet() {
        let (_d, home) = home();
        let repo = repo();
        let id = "t1";
        worktree(&home, repo.path(), id);
        let (task, eff) = task(repo.path(), id);
        snapshot(&home, &task, &eff);

        assert!(escaped_paths(&home, &task, &eff).is_none());
        assert!(health(&home, true).is_empty());
        assert!(!log_of(&home, id).contains("base checkout"));
    }

    #[test]
    fn a_base_checkout_already_dirty_is_not_blamed_again() {
        let (_d, home) = home();
        let repo = repo();
        let id = "t1";
        worktree(&home, repo.path(), id);
        // Someone had uncommitted work in the checkout before the task ran.
        std::fs::write(repo.path().join("already.txt"), "x").unwrap();
        let (task, eff) = task(repo.path(), id);
        snapshot(&home, &task, &eff);

        assert!(escaped_paths(&home, &task, &eff).is_none());
        assert!(health(&home, true).is_empty());
    }

    #[test]
    fn work_in_the_worktree_is_not_an_escape_even_if_the_base_changed() {
        let (_d, home) = home();
        let repo = repo();
        let id = "t1";
        let wt = worktree(&home, repo.path(), id);
        let (task, eff) = task(repo.path(), id);
        snapshot(&home, &task, &eff);

        std::fs::write(wt.join("real-work.txt"), "x").unwrap();
        std::fs::write(repo.path().join("unrelated.txt"), "x").unwrap();

        assert!(escaped_paths(&home, &task, &eff).is_none());
    }

    #[test]
    fn a_warning_clears_once_the_base_checkout_does() {
        let (_d, home) = home();
        let repo = repo();
        let id = "t1";
        worktree(&home, repo.path(), id);
        let (task, eff) = task(repo.path(), id);
        snapshot(&home, &task, &eff);
        std::fs::write(repo.path().join("escaped.txt"), "x").unwrap();
        let paths = escaped_paths(&home, &task, &eff).unwrap();
        report(&home, &task, &eff, &paths);
        assert_eq!(health(&home, true).len(), 1);

        std::fs::remove_file(repo.path().join("escaped.txt")).unwrap();
        assert!(health(&home, true).is_empty());
        assert!(
            !record_file(&home, id, 1).exists(),
            "the stale marker is gone"
        );
    }
}
