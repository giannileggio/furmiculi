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
/// Reading `origin`: short, because the dashboard asks for it on a request.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(5);
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

/// The repo's identity across machines: its `origin` URL reduced to
/// `host/owner/name`, so every spelling of the same remote compares equal.
/// `None` if there is no `origin`, or it is a local path.
pub fn remote_id(repo: &Path) -> Option<String> {
    let out = run(
        "git",
        repo,
        &["remote", "get-url", "origin"],
        REMOTE_TIMEOUT,
    )
    .ok()?;
    normalize_remote(out.trim())
}

/// `git@github.com:me/app.git`, `https://github.com/me/app/` and
/// `ssh://git@github.com:22/me/app.git` all become `github.com/me/app`.
/// The host is lowercased; the path keeps its case. Local paths (including
/// `file://` and Windows drive letters), and anything that would carry
/// credentials into the id, have no identity.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let (authority, path) = match url.split_once("://") {
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("file") => return None,
        Some((_, rest)) => rest.split_once('/')?,
        None => {
            // git's rule: scp-like when a `:` comes before any `/` (looking
            // past a bracketed IPv6 host), and a one-letter "host" is a drive.
            let from = url.find(']').filter(|&i| url[..i].find('/').is_none());
            let colon = from.unwrap_or(0) + url[from.unwrap_or(0)..].find(':')?;
            if url[..colon].contains('/') || url[..colon].len() < 2 {
                return None;
            }
            (&url[..colon], &url[colon + 1..])
        }
    };
    let host = authority.rsplit('@').next()?;
    let host = match host.strip_prefix('[') {
        Some(v6) => format!("[{}]", v6.split(']').next()?),
        None => host.split(':').next()?.to_string(),
    }
    .to_lowercase();
    let path = path.split(['?', '#']).next()?.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    // A raw `/` in a password lands the secret in the host or path.
    if host.is_empty() || path.is_empty() || host.contains('@') || path.contains('@') {
        return None;
    }
    Some(format!("{host}/{path}"))
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
#[cfg(test)]
pub fn ensure_worktree(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    ensure_worktree_from(repo, path, branch, None)
}

/// Brings `base` up to date if it lives on a remote (`origin/main`), and
/// checks that it names a commit. Being offline isn't fatal: a base the repo
/// already has is used as it stands, a moment stale.
fn prepare_base(repo: &Path, base: &str) -> Result<()> {
    if let Some((remote, _)) = base.split_once('/')
        && git(repo, &["remote"])?.lines().any(|r| r.trim() == remote)
    {
        let _ = run("git", repo, &["fetch", "--quiet", remote], NETWORK_TIMEOUT);
    }
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{base}^{{commit}}"),
        ],
    )
    .map(|_| ())
    .map_err(|_| {
        anyhow::anyhow!(
            "base `{base}` is not a branch, tag or commit of {}",
            repo.display()
        )
    })
}

/// [`ensure_worktree`], with a new branch starting from `base` (see
/// [`prepare_base`]) instead of the repo's HEAD. A branch or worktree that
/// already exists is left where it is: `base` is where work starts, not
/// something a retry rebases onto.
pub fn ensure_worktree_from(
    repo: &Path,
    path: &Path,
    branch: &str,
    base: Option<&str>,
) -> Result<()> {
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
    } else if let Some(base) = base {
        prepare_base(repo, base)?;
        // --no-track: the branch is the task's own, pushed under its own name.
        git(
            repo,
            &["worktree", "add", "--no-track", "-b", branch, p, base],
        )?;
    } else {
        git(repo, &["worktree", "add", "-b", branch, p])?;
    }
    Ok(())
}

/// Checks out `branch` in `repo` itself, creating it if needed. Refuses to
/// switch branches over uncommitted changes, which would carry them onto
/// the task's branch (or fail halfway).
#[cfg(test)]
pub fn checkout_branch(repo: &Path, branch: &str) -> Result<()> {
    checkout_branch_from(repo, branch, None)
}

pub fn checkout_branch_from(repo: &Path, branch: &str, base: Option<&str>) -> Result<()> {
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
    } else if let Some(base) = base {
        prepare_base(repo, base)?;
        git(repo, &["checkout", "--no-track", "-b", branch, base])?;
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
/// [`check_broken_worktree`] holds the refusals, so a dry run can say
/// what this would and wouldn't take.
pub fn remove_broken_worktree(path: &Path) -> Result<()> {
    check_broken_worktree(path)?;
    // Best effort: forget the worktree in a repo that is still
    // reachable. When the repo is gone there is nothing left that
    // references the worktree, and nothing to prune.
    let _ = git(path, &["worktree", "prune"]);
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    Ok(())
}

/// Whether [`remove_broken_worktree`] would remove `path`.
///
/// Only a linked worktree root is removable this way: its `.git`
/// is a file naming the (unreachable) gitdir, and `git rev-parse
/// --show-toplevel` either fails (the repo is gone) or names the
/// directory itself. A plain directory, a checkout of its own, or
/// a subdirectory of a larger checkout is refused — removing any
/// of those would delete unrelated work.
pub fn check_broken_worktree(path: &Path) -> Result<()> {
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

/// Whether `branch` exists in `repo` and is fully merged into `base`.
pub fn branch_merged_into(repo: &Path, branch: &str, base: &str) -> bool {
    branch_exists(repo, branch)
        && branch_exists(repo, base)
        && git(
            repo,
            &[
                "merge-base",
                "--is-ancestor",
                &format!("refs/heads/{branch}"),
                &format!("refs/heads/{base}"),
            ],
        )
        .is_ok()
}

/// `git branch -d`: deletes a branch only if git agrees it is merged.
pub fn delete_merged_branch(repo: &Path, branch: &str) -> Result<()> {
    git(repo, &["branch", "-d", branch]).map(drop)
}

/// The repo's main branch: what `origin/HEAD` points at, else `main` or `master`.
pub fn main_branch(repo: &Path) -> Option<String> {
    if let Ok(out) = git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) && let Some(name) = out.trim().strip_prefix("origin/")
        && branch_exists(repo, name)
    {
        return Some(name.to_string());
    }
    ["main", "master"]
        .into_iter()
        .find(|b| branch_exists(repo, b))
        .map(String::from)
}

pub fn has_changes(dir: &Path) -> Result<bool> {
    Ok(!git(dir, &["status", "--porcelain"])?.trim().is_empty())
}

/// The paths `git status --porcelain` reports as changed in `dir`, one
/// `git` line each (` M src/lib.rs`, `?? new.txt`), so a caller can say what
/// `on_finish` is about to commit.
pub fn changed_paths(dir: &Path) -> Result<Vec<String>> {
    Ok(git(dir, &["status", "--porcelain"])?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(String::from)
        .collect())
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

/// A new file bigger than this is not something an agent's commit should
/// carry without anyone looking.
const GUARD_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Does this file name look like it holds a secret?
fn looks_secret(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let example = [".example", ".sample", ".template", ".dist", ".defaults"]
        .iter()
        .any(|s| name.ends_with(s));
    if example {
        return false;
    }
    name == ".env"
        || name.starts_with(".env.")
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"]
            .iter()
            .any(|s| name.ends_with(s))
        || [
            "id_rsa",
            "id_dsa",
            "id_ecdsa",
            "id_ed25519",
            ".npmrc",
            ".netrc",
            ".pypirc",
            "credentials.json",
            "service-account.json",
            "secrets.json",
            "secrets.yaml",
            "secrets.yml",
        ]
        .contains(&name.as_str())
}

/// New files in `dir` that a commit of everything would take and probably
/// shouldn't: ones named like secrets, and very large ones. Files git
/// already tracks are the repo's own business.
pub fn guarded_paths(dir: &Path) -> Result<Vec<String>> {
    let status = git(dir, &["status", "--porcelain", "--untracked-files=all"])?;
    let mut out = Vec::new();
    for line in status.lines() {
        let (Some(state), Some(path)) = (line.get(..2), line.get(3..)) else {
            continue;
        };
        if !(state == "??" || state.starts_with('A')) {
            continue;
        }
        let path = path.trim().trim_matches('"');
        if looks_secret(path) {
            out.push(format!("{path} (looks like a secret)"));
        } else if let Ok(meta) = std::fs::metadata(dir.join(path))
            && meta.len() > GUARD_MAX_BYTES
        {
            out.push(format!("{path} ({} MB)", meta.len() / (1024 * 1024)));
        }
    }
    Ok(out)
}

/// Runs the task's `on_finish` step in `dir`. With `guard`, it first refuses
/// a commit that would take a file that looks like a secret or is very large
/// (see [`guarded_paths`]): the work stays in `dir` for someone to look at.
pub fn on_finish_guarded(
    action: OnFinish,
    dir: &Path,
    id: &str,
    prompt: &str,
    guard: bool,
) -> Result<()> {
    if guard && action != OnFinish::None {
        let held = guarded_paths(dir)?;
        if !held.is_empty() {
            let shown = held.len().min(8);
            let more = if held.len() > shown {
                format!(" and {} more", held.len() - shown)
            } else {
                String::new()
            };
            bail!(
                "not committing: the changes include {}{more}. Remove or .gitignore them in {} \
                 and `furmi done {id}`, or set `commit_guard = false` under [defaults] in \
                 config.toml if they belong in the repo",
                held[..shown].join(", "),
                dir.display()
            );
        }
    }
    on_finish(action, dir, id, prompt)
}

/// Runs the task's `on_finish` step in `dir`.
pub fn on_finish(action: OnFinish, dir: &Path, id: &str, prompt: &str) -> Result<()> {
    if action == OnFinish::None {
        return Ok(());
    }
    // A task can legitimately change nothing ("remove the watermark if
    // there is one"). There is then no branch to push and nothing to open a
    // PR for: `gh pr create` would fail with "No commits between ...",
    // turning a successful run into a failed one. Nothing to do is success.
    let message = commit_message(id, prompt);
    if !commit_all(dir, &message)? {
        return Ok(());
    }
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
/// Whether the branch in `dir` has been pushed with commits of its own: the
/// cheap local test for "there may be a pull request to ask about".
pub fn has_commits_ahead(dir: &Path) -> bool {
    git(
        dir,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .is_ok()
}

/// The address of the pull request for the branch checked out in `dir`.
pub fn pr_url(dir: &Path) -> Option<String> {
    run(
        "gh",
        dir,
        &["pr", "view", "--json", "url", "--jq", ".url"],
        NETWORK_TIMEOUT,
    )
    .ok()
    .map(|url| url.trim().to_string())
    .filter(|url| url.starts_with("https://"))
}

/// What a task changed, for the dashboard's task view.
#[derive(Debug, Default, serde::Serialize)]
pub struct Changes {
    /// `git diff --shortstat`'s line: "3 files changed, 10 insertions(+)...".
    pub summary: String,
    /// `status path` lines, committed ones first; capped.
    pub files: Vec<String>,
    /// How many files there are in all, when `files` was cut short.
    pub total: usize,
    /// Changes in the workspace that no commit holds yet.
    pub uncommitted: usize,
}

/// Most files listed in a task's view.
const CHANGES_SHOWN: usize = 60;

/// What `branch` holds that the repo's checked-out branch doesn't, plus
/// whatever is uncommitted in `workdir` (the task's worktree, if it still
/// exists). `None` when git has nothing to say (no such branch, not a repo).
pub fn changes(repo: &Path, workdir: Option<&Path>, branch: Option<&str>) -> Option<Changes> {
    let mut out = Changes::default();
    if let Some(branch) = branch.filter(|b| branch_exists(repo, b)) {
        let range = format!("HEAD...{branch}");
        if let Ok(names) = git(repo, &["diff", "--name-status", &range]) {
            out.files.extend(
                names
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.replace('\t', " ")),
            );
        }
        if let Ok(stat) = git(repo, &["diff", "--shortstat", &range]) {
            out.summary = stat.trim().to_string();
        }
    }
    if let Some(dir) = workdir.filter(|d| d.is_dir())
        && let Ok(paths) = changed_paths(dir)
    {
        out.uncommitted = paths.len();
        out.files
            .extend(paths.into_iter().map(|p| format!("{p} (not committed)")));
    }
    if out.files.is_empty() && out.summary.is_empty() {
        return None;
    }
    out.total = out.files.len();
    out.files.truncate(CHANGES_SHOWN);
    Some(out)
}

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
    fn every_spelling_of_a_remote_has_one_identity() {
        for url in [
            "git@github.com:me/app.git",
            "https://github.com/me/app",
            "https://github.com/me/app.git/",
            "https://user:pw@GitHub.com/me/app.git",
            "ssh://git@github.com:22/me/app.git",
            "https://github.com/me/app?x=1#frag",
        ] {
            assert_eq!(
                normalize_remote(url).as_deref(),
                Some("github.com/me/app"),
                "{url}"
            );
        }
        for local in [
            "/srv/git/app.git",
            "../app",
            "~/app",
            "file:///srv/app.git",
            "file://localhost/srv/app.git",
            "C:/src/app",
            "C:\\src\\app",
            "srv/a:b",
            "https://user:p/w@github.com/me/app",
            "",
        ] {
            assert_eq!(normalize_remote(local), None, "{local}");
        }
    }

    #[test]
    fn an_ipv6_host_keeps_its_address() {
        for (url, id) in [
            ("ssh://git@[::1]:22/me/app.git", "[::1]/me/app"),
            ("ssh://git@[2001:db8::2]/me/app", "[2001:db8::2]/me/app"),
            ("git@[fe80::1]:me/app.git", "[fe80::1]/me/app"),
        ] {
            assert_eq!(normalize_remote(url).as_deref(), Some(id), "{url}");
        }
    }

    #[test]
    fn a_repo_is_identified_by_its_origin() {
        let repo = repo();
        assert_eq!(remote_id(repo.path()), None);
        git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:me/app.git"],
        )
        .unwrap();
        assert_eq!(remote_id(repo.path()).as_deref(), Some("github.com/me/app"));
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
    fn changed_paths_lists_what_a_commit_would_take() {
        let repo = repo();
        let p = repo.path();
        std::fs::write(p.join("tracked.txt"), "a").unwrap();
        git(p, &["add", "-A"]).unwrap();
        git(p, &["commit", "-q", "-m", "tracked"]).unwrap();
        std::fs::write(p.join("tracked.txt"), "b").unwrap();
        std::fs::write(p.join("untracked.txt"), "c").unwrap();
        let paths = changed_paths(p).unwrap();
        assert_eq!(paths, [" M tracked.txt", "?? untracked.txt"]);
        std::fs::remove_file(p.join("untracked.txt")).unwrap();
        git(p, &["checkout", "--", "tracked.txt"]).unwrap();
        assert!(changed_paths(p).unwrap().is_empty());
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
    fn a_clean_tree_makes_pr_on_finish_succeed_without_a_pr() {
        let repo = repo();
        let p = repo.path();
        // There is no `origin` to push to and no `gh` to open a PR with, so
        // reaching either would fail: a clean tree must skip both.
        on_finish(OnFinish::Pr, p, "t", "Remove the watermark if there is one").unwrap();
        assert_eq!(
            git(p, &["rev-list", "--count", "HEAD"]).unwrap().trim(),
            "1"
        );
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

    #[test]
    fn a_new_branch_can_start_from_a_base_instead_of_head() {
        let repo = repo();
        let p = repo.path();
        git(p, &["branch", "release"]).unwrap();
        std::fs::write(p.join("later.txt"), "x").unwrap();
        git(p, &["add", "-A"]).unwrap();
        git(p, &["commit", "-q", "-m", "later"]).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        ensure_worktree_from(p, &wt, "furmi/based", Some("release")).unwrap();
        assert!(
            !wt.join("later.txt").exists(),
            "started from HEAD, not the base"
        );

        let err = ensure_worktree_from(p, &dir.path().join("wt2"), "furmi/nope", Some("no-such"))
            .unwrap_err();
        assert!(err.to_string().contains("base `no-such`"), "{err}");
    }

    #[test]
    fn the_commit_guard_holds_new_secrets_and_leaves_tracked_files_alone() {
        let repo = repo();
        let p = repo.path();
        std::fs::write(p.join(".env"), "tracked on purpose").unwrap();
        git(p, &["add", "-A"]).unwrap();
        git(p, &["commit", "-q", "-m", "env"]).unwrap();
        std::fs::write(p.join(".env"), "changed").unwrap();
        std::fs::write(p.join("code.rs"), "fn main() {}").unwrap();
        std::fs::write(p.join(".env.example"), "KEY=").unwrap();
        assert!(guarded_paths(p).unwrap().is_empty());

        std::fs::create_dir(p.join("deploy")).unwrap();
        std::fs::write(p.join("deploy/id_rsa"), "key").unwrap();
        std::fs::write(p.join(".env.production"), "SECRET=1").unwrap();
        let held = guarded_paths(p).unwrap();
        assert_eq!(held.len(), 2, "{held:?}");

        let err = on_finish_guarded(OnFinish::Commit, p, "t", "do it", true).unwrap_err();
        assert!(err.to_string().contains("deploy/id_rsa"), "{err}");
        assert!(has_changes(p).unwrap(), "nothing was committed");
        on_finish_guarded(OnFinish::Commit, p, "t", "do it", false).unwrap();
        assert!(!has_changes(p).unwrap());
    }
}
