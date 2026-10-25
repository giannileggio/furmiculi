//! Git and `gh` operations the runner needs, run as argv (no shell).
//!
//! Every call has a timeout and no stdin, and git/gh are told never to
//! prompt, so a stalled network or a credential prompt fails the call
//! instead of freezing the runner.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::fsutil::run_with_timeout;
use crate::spec::OnFinish;

/// Local git commands (status, commit, worktree...).
const LOCAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Network commands: `git push`, `gh`.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

fn run(program: &str, dir: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_EDITOR", "true");
    let out = run_with_timeout(cmd, timeout).map_err(|e| {
        // `spawn` failures are io errors; a timeout is a plain message.
        match e.downcast_ref::<std::io::Error>() {
            Some(_) => e.context(format!("running {program} (is it installed?)")),
            None => e.context(format!("`{program} {}`", args.join(" "))),
        }
    })?;
    if !out.status.success() {
        bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    run("git", dir, args, LOCAL_TIMEOUT)
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// The top-level folder of the git repo containing `dir`, if there is one.
pub fn repo_root(dir: &Path) -> Result<std::path::PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"])?;
    Ok(std::path::PathBuf::from(out.trim()))
}

/// One key per repository, whatever spelling `repo` has: the top level of
/// the checkout containing it when it is inside a git repo, else the path as
/// given. So `~/app`, `~/app/.`, a path through a symlink and `~/app/sub`
/// all key the same repo.
pub fn repo_key(repo: &Path) -> PathBuf {
    repo_root(repo).unwrap_or_else(|_| repo.to_path_buf())
}

/// The repo's shared `.git` directory, as an absolute path. The same for a
/// repo and all of its linked worktrees.
fn common_dir(dir: &Path) -> Result<std::path::PathBuf> {
    let out = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let path = Path::new(out.trim());
    Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

/// Makes `path` a worktree of `repo` on `branch`, creating the branch from
/// the repo's HEAD if needed. An existing `path` is reused as is (whatever
/// branch the agent left it on), so a retried task picks up where the
/// previous attempt left off, but only if it really is a worktree of `repo`.
pub fn ensure_worktree(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    if path.exists() {
        let ours = common_dir(path).ok().zip(common_dir(repo).ok());
        return match ours {
            Some((a, b)) if a == b => Ok(()),
            _ => bail!(
                "{} exists but is not a git worktree of {}; remove it (or its task) and retry",
                path.display(),
                repo.display()
            ),
        };
    }
    let p = path.to_str().context("worktree path is not valid UTF-8")?;
    // A worktree directory deleted by hand leaves a stale registration that
    // would make `worktree add` refuse the same path or branch.
    git(repo, &["worktree", "prune"])?;
    if branch_exists(repo, branch) {
        git(repo, &["worktree", "add", p, branch])?;
    } else {
        git(repo, &["worktree", "add", "-b", branch, p])?;
    }
    Ok(())
}

/// Checks out `branch` in `repo` itself, creating it if needed. Refuses to
/// switch branches over uncommitted changes, which would carry them onto
/// the task's branch (or fail halfway).
pub fn checkout_branch(repo: &Path, branch: &str) -> Result<()> {
    let current = git(repo, &["branch", "--show-current"])?;
    if current.trim() == branch {
        return Ok(());
    }
    if has_changes(repo)? {
        bail!(
            "{} has uncommitted changes: commit or stash them, or run the task in a worktree",
            repo.display()
        );
    }
    if branch_exists(repo, branch) {
        git(repo, &["checkout", branch])?;
    } else {
        git(repo, &["checkout", "-b", branch])?;
    }
    Ok(())
}

/// Removes the linked worktree at `path` from whichever repo owns it. Its
/// branch stays. Like `git worktree remove`, refuses if it has changes.
pub fn remove_worktree(path: &Path) -> Result<()> {
    let common = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let p = path.to_str().context("worktree path is not valid UTF-8")?;
    git(Path::new(common.trim()), &["worktree", "remove", p])?;
    Ok(())
}

/// Removes a worktree `git worktree remove` can't touch: one whose
/// owning repo moved or was deleted, so its gitdir is unreachable
/// and every git command in it fails — `furmi clean` would keep it
/// forever. `git worktree prune` clears any registration a
/// surviving repo still holds (best effort: it fails like every
/// other git command when the repo is gone), then the directory is
/// removed directly.
///
/// Only a linked worktree root is removed this way: its `.git`
/// is a file naming the (unreachable) gitdir, and `git rev-parse
/// --show-toplevel` either fails (the repo is gone) or names the
/// directory itself. A plain directory, a checkout of its own, or
/// a subdirectory of a larger checkout is refused — removing any
/// of those would delete unrelated work.
pub fn remove_broken_worktree(path: &Path) -> Result<()> {
    if !path.join(".git").is_file() {
        bail!(
            "{} is not a git worktree; remove it by hand",
            path.display()
        );
    }
    if let Ok(root) = repo_root(path)
        && root != path
    {
        bail!(
            "{} is part of the checkout at {}; remove it by hand",
            path.display(),
            root.display()
        );
    }
    // Best effort: forget the worktree in a repo that is still
    // reachable. When the repo is gone there is nothing left that
    // references the worktree, and nothing to prune.
    let _ = git(path, &["worktree", "prune"]);
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    Ok(())
}

/// Whether `name` is a branch name git will actually accept, per
/// `git check-ref-format`: no whitespace or control characters, no `..`,
/// `~`, `^`, `:`, `?`, `*`, `[`, no `@{`, no leading/trailing/doubled `/`,
/// no trailing `.` or `.lock`. Needs no repository.
pub fn valid_branch_name(name: &str) -> bool {
    Command::new("git")
        .args(["check-ref-format", &format!("refs/heads/{name}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn has_changes(dir: &Path) -> Result<bool> {
    Ok(!git(dir, &["status", "--porcelain"])?.trim().is_empty())
}

/// Commits everything in `dir`. Returns false if there was nothing to commit.
pub fn commit_all(dir: &Path, message: &str) -> Result<bool> {
    if !has_changes(dir)? {
        return Ok(false);
    }
    git(dir, &["add", "-A"])?;
    git(dir, &["commit", "-q", "-m", message])?;
    Ok(true)
}

/// Commit message for a task: the prompt's first line as the subject (cut
/// to 72 chars), the whole prompt as the body.
pub fn commit_message(id: &str, prompt: &str) -> String {
    let first = prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(id)
        .trim();
    let subject: String = if first.chars().count() > 72 {
        first.chars().take(71).chain(['…']).collect()
    } else {
        first.to_string()
    };
    if prompt.trim() == subject {
        subject
    } else {
        format!("{subject}\n\n{}\n\nfurmi task: {id}", prompt.trim())
    }
}

/// Runs the task's `on_finish` step in `dir`.
pub fn on_finish(action: OnFinish, dir: &Path, id: &str, prompt: &str) -> Result<()> {
    if action == OnFinish::None {
        return Ok(());
    }
    let message = commit_message(id, prompt);
    commit_all(dir, &message)?;
    if action == OnFinish::Commit {
        return Ok(());
    }
    run(
        "git",
        dir,
        &["push", "-u", "origin", "HEAD"],
        NETWORK_TIMEOUT,
    )?;
    if action == OnFinish::Pr {
        // The push may have worked on an earlier attempt whose `gh` call
        // then failed: don't open a second PR for the same branch. But a
        // stale `furmi/<id>` branch from an earlier task can still have a PR
        // of its own — closed, or for an older commit — which `gh pr view`
        // reports for this branch: only an open PR at this run's head
        // commit counts as already opened.
        if pr_covers_head(dir)? {
            return Ok(());
        }
        let (title, body) = message.split_once("\n\n").unwrap_or((&message, ""));
        run(
            "gh",
            dir,
            &["pr", "create", "--title", title, "--body", body],
            NETWORK_TIMEOUT,
        )?;
    }
    Ok(())
}

/// Whether the current branch already has an open PR whose head is the
/// branch's current commit, so a second one must not be opened. `gh pr
/// view` alone is not enough: it also succeeds for a stale `furmi/<id>`
/// branch from an earlier task, whose PR may be closed or point at an
/// older commit — neither covers this run's work. `gh` failing to answer
/// (e.g. no PR for the branch at all) counts as no covering PR; the
/// `pr create` that follows then reports the real problem, if any.
fn pr_covers_head(dir: &Path) -> Result<bool> {
    let head = git(dir, &["rev-parse", "HEAD"])?;
    let out = run(
        "gh",
        dir,
        &[
            "pr",
            "view",
            "--json",
            "state,headRefOid",
            "--jq",
            "[.state, .headRefOid] | join(\" \")",
        ],
        NETWORK_TIMEOUT,
    );
    Ok(match out {
        Ok(text) => {
            let mut parts = text.trim().split(' ');
            parts.next() == Some("OPEN") && parts.next() == Some(head.trim())
        }
        Err(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Test"],
            &["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            git(p, args).unwrap();
        }
        dir
    }

    fn current_branch(dir: &Path) -> String {
        git(dir, &["branch", "--show-current"])
            .unwrap()
            .trim()
            .into()
    }

    #[test]
    fn one_key_per_repo_however_its_path_is_spelled() {
        let repo = repo();
        let root = repo.path().canonicalize().unwrap();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();

        assert_eq!(repo_key(&root), root);
        // `Path` equality already ignores `.` components; every spelling
        // still has to come back as the same, real root.
        assert_eq!(repo_key(&root.join(".")), root);
        assert_eq!(repo_key(&sub), root);
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert_eq!(repo_key(&link), root);

        // Not a checkout: the path is kept as given.
        let plain = tempfile::tempdir().unwrap();
        let plain = plain.path().canonicalize().unwrap();
        assert_eq!(repo_key(&plain), plain);
    }

    #[test]
    fn creates_and_reuses_worktrees() {
        let repo = repo();
        let wt = repo.path().join("../").join(format!(
            "{}-wt",
            repo.path().file_name().unwrap().to_str().unwrap()
        ));
        ensure_worktree(repo.path(), &wt, "furmi/new").unwrap();
        assert_eq!(current_branch(&wt), "furmi/new");

        std::fs::write(wt.join("f.txt"), "x").unwrap();
        ensure_worktree(repo.path(), &wt, "furmi/new").unwrap();
        assert!(wt.join("f.txt").exists(), "existing worktree is reused");

        // An existing branch is checked out rather than recreated.
        git(repo.path(), &["branch", "existing"]).unwrap();
        let wt2 = wt.with_extension("2");
        ensure_worktree(repo.path(), &wt2, "existing").unwrap();
        assert_eq!(current_branch(&wt2), "existing");

        std::fs::remove_dir_all(&wt).unwrap();
        std::fs::remove_dir_all(&wt2).unwrap();
    }

    #[test]
    fn removes_clean_worktrees_and_keeps_branches() {
        let repo = repo();
        let wt = repo.path().join("wt");
        ensure_worktree(repo.path(), &wt, "furmi/gone").unwrap();

        std::fs::write(wt.join("f.txt"), "x").unwrap();
        assert!(remove_worktree(&wt).is_err(), "dirty worktrees stay");
        std::fs::remove_file(wt.join("f.txt")).unwrap();

        remove_worktree(&wt).unwrap();
        assert!(!wt.exists());
        assert!(branch_exists(repo.path(), "furmi/gone"));
        assert!(
            !git(repo.path(), &["worktree", "list"])
                .unwrap()
                .contains("wt")
        );
    }

    #[test]
    fn checks_out_branches_in_place() {
        let repo = repo();
        checkout_branch(repo.path(), "feature").unwrap();
        assert_eq!(current_branch(repo.path()), "feature");
        checkout_branch(repo.path(), "main").unwrap();
        assert_eq!(current_branch(repo.path()), "main");
    }

    #[test]
    fn checkout_refuses_to_carry_uncommitted_changes() {
        let repo = repo();
        let p = repo.path();
        std::fs::write(p.join("wip.txt"), "x").unwrap();
        let err = checkout_branch(p, "feature").unwrap_err();
        assert!(err.to_string().contains("uncommitted"), "{err}");
        assert_eq!(current_branch(p), "main");
        // Already on the branch: nothing to switch, so nothing to refuse.
        checkout_branch(p, "main").unwrap();
    }

    #[test]
    fn worktree_paths_must_really_be_worktrees_of_the_repo() {
        let other = repo();
        let repo = repo();
        let stray = tempfile::tempdir().unwrap();
        let err = ensure_worktree(repo.path(), stray.path(), "furmi/x").unwrap_err();
        assert!(err.to_string().contains("not a git worktree"), "{err}");

        // A worktree of some other repo isn't ours either.
        let foreign = other.path().join("wt");
        ensure_worktree(other.path(), &foreign, "furmi/y").unwrap();
        assert!(ensure_worktree(repo.path(), &foreign, "furmi/y").is_err());
    }

    #[test]
    fn a_worktree_deleted_by_hand_can_be_recreated() {
        let repo = repo();
        let wt = repo.path().join("wt");
        ensure_worktree(repo.path(), &wt, "furmi/again").unwrap();
        std::fs::remove_dir_all(&wt).unwrap(); // leaves a stale registration
        ensure_worktree(repo.path(), &wt, "furmi/again").unwrap();
        assert_eq!(current_branch(&wt), "furmi/again");
    }

    #[test]
    fn hung_git_commands_time_out() {
        let repo = repo();
        let hook = repo.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nsleep 30\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(repo.path().join("f.txt"), "x").unwrap();
        git(repo.path(), &["add", "-A"]).unwrap();
        let err = run(
            "git",
            repo.path(),
            &["commit", "-m", "slow"],
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
    }

    #[test]
    fn commit_on_finish_commits_only_when_dirty() {
        let repo = repo();
        let p = repo.path();
        on_finish(OnFinish::Commit, p, "t", "Nothing to do").unwrap();
        assert_eq!(
            git(p, &["rev-list", "--count", "HEAD"]).unwrap().trim(),
            "1"
        );

        std::fs::write(p.join("new.txt"), "hi").unwrap();
        on_finish(OnFinish::Commit, p, "t", "Add a file\n\nWith details.").unwrap();
        assert!(!has_changes(p).unwrap());
        let log = git(p, &["log", "-1", "--format=%B"]).unwrap();
        assert!(log.starts_with("Add a file\n\nAdd a file\n\nWith details."));
        assert!(log.contains("furmi task: t"));
    }

    #[test]
    fn validates_branch_names() {
        for ok in ["fix/login", "valid-name123", "a"] {
            assert!(valid_branch_name(ok), "{ok} should be valid");
        }
        for bad in [
            "",
            " ",
            "a b",
            "..",
            "trailing.",
            "trailing/",
            "a//b",
            "ends.lock",
            "a~b",
            "@{x}",
        ] {
            assert!(!valid_branch_name(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn commit_message_shapes() {
        assert_eq!(commit_message("t", "Short one"), "Short one");
        let long = "x".repeat(100);
        let subject = commit_message("t", &long)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(subject.chars().count(), 72);
        assert!(subject.ends_with('…'));
    }
}
