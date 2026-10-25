//! End-to-end tests of the runner (`furmi run`) against the built binary, with
//! a stub "agent" that just runs the task's prompt as a shell command.
//!
//! Each test gets its own home, its own git repo and its own tmux server
//! (via `TMUX_TMPDIR`), so nothing touches the real user's tmux or files.
//! Needs `tmux` and `git`, like the runner itself.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Env {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    tmux: tempfile::TempDir,
}

/// The tmux session tasks run in (see the config `Env::new` writes).
const SESSION: &str = "furmit";

/// The binary under test. `CARGO_BIN_EXE_furmi` is baked in at compile time,
/// and cargo doesn't rebuild a test just because that path changed, so a test
/// binary built before the checkout moved (it did: see docs/RENAME.md) spawns
/// a path that no longer exists. Fall back to the `furmi` cargo just built
/// next to the deps folder, i.e. `target/debug/deps/<test>` → `target/debug`.
fn furmi_bin() -> PathBuf {
    let baked = PathBuf::from(env!("CARGO_BIN_EXE_furmi"));
    if baked.exists() {
        return baked;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("furmi")))
        .filter(|path| path.exists())
        .unwrap_or(baked)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("running git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

impl Env {
    fn new(extra_config: &str) -> Env {
        let env = Env {
            home: tempfile::tempdir().unwrap(),
            repo: tempfile::tempdir().unwrap(),
            tmux: tempfile::tempdir().unwrap(),
        };
        ok(env
            .furmi()
            .args(["init", "--agent", "claude", "--no-global-skill"])
            .output()
            .unwrap());
        std::fs::write(
            env.home.path().join("config.toml"),
            format!(
                "[runner]\ntmux_session = \"{SESSION}\"\npoll_interval = \"1s\"\n{extra_config}\n\
                 [defaults]\nagent = \"stub\"\nretry_delay = \"1s\"\n\n\
                 [agents.stub]\nheadless = [\"sh\", \"-c\", \"{{prompt}}\"]\n\
                 interactive = [\"sh\", \"-c\", \"{{prompt}}\"]\n"
            ),
        )
        .unwrap();
        let repo = env.repo.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test"]);
        git(repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        env
    }

    fn furmi(&self) -> Command {
        let mut cmd = Command::new(furmi_bin());
        cmd.env("HOME", self.home.path())
            .env("TMUX_TMPDIR", self.tmux.path())
            .env_remove("TMUX")
            .env("FURMI_NO_UPDATE_CHECK", "1")
            .arg("--home")
            .arg(self.home.path())
            .stdin(Stdio::null());
        cmd
    }

    fn furmi_ok(&self, args: &[&str]) -> Output {
        ok(self.furmi().args(args).output().unwrap())
    }

    /// Queues a task running `prompt` in the test repo.
    fn add(&self, id: &str, prompt: &str, extra: &[&str]) {
        let repo = self.repo.path().display().to_string();
        let mut args = vec!["add", "--repo", &repo, "--id", id, "--prompt", prompt];
        args.extend(extra);
        self.furmi_ok(&args);
    }

    /// One `furmi run --once` pass.
    fn tick(&self) {
        self.furmi_ok(&["run", "--once"]);
    }

    fn task_path(&self, id: &str) -> Option<PathBuf> {
        ["tasks", "archive"]
            .iter()
            .map(|d| self.home.path().join(d).join(format!("{id}.md")))
            .find(|p| p.exists())
    }

    /// A frontmatter value of the task, and whether it's archived.
    fn field(&self, id: &str, key: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.task_path(id)?).ok()?;
        text.lines()
            .skip(1)
            .take_while(|l| *l != "---")
            .find_map(|l| l.strip_prefix(&format!("{key}: ")).map(str::to_string))
    }

    fn archived(&self, id: &str) -> bool {
        self.home
            .path()
            .join("archive")
            .join(format!("{id}.md"))
            .exists()
    }

    fn worktree(&self, id: &str) -> PathBuf {
        self.home.path().join("worktrees").join(id)
    }

    /// Ticks until `id` is archived, and returns its final status.
    fn run_until_archived(&self, id: &str) -> String {
        wait_for(
            &format!("{id} to be archived"),
            Duration::from_secs(60),
            || {
                self.tick();
                self.archived(id)
            },
        );
        self.field(id, "status").unwrap()
    }

    fn tmux_kill_server(&self) {
        let _ = Command::new("tmux")
            .arg("kill-server")
            .env("TMUX_TMPDIR", self.tmux.path())
            .env_remove("TMUX")
            .output();
    }

    /// Kills the session tasks run in, leaving the server (and any
    /// other sessions) alone.
    fn tmux_kill_session(&self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &format!("={SESSION}")])
            .env("TMUX_TMPDIR", self.tmux.path())
            .env_remove("TMUX")
            .output();
    }

    /// Runs `furmi run` in the background, like `furmi start` does.
    fn runner(&self) -> std::process::Child {
        self.furmi()
            .arg("run")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.tmux_kill_server();
    }
}

fn ok(out: Output) -> Output {
    assert!(
        out.status.success(),
        "expected success, got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Whether a process is still running (a zombie doesn't count).
fn alive(pid: &str) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

#[test]
fn a_task_runs_to_done_and_its_work_is_committed() {
    let env = Env::new("");
    env.add(
        "happy",
        "echo hello-from-agent; echo data > out.txt",
        &["--on-finish", "commit"],
    );

    assert_eq!(env.run_until_archived("happy"), "done");
    assert_eq!(env.field("happy", "attempts").as_deref(), Some("1"));

    let log = std::fs::read_to_string(env.home.path().join("logs/happy.1.log")).unwrap();
    assert!(log.contains("hello-from-agent"), "{log}");
    let wt = env.worktree("happy");
    assert_eq!(
        git(&wt, &["show", "--name-only", "--format=", "HEAD"]),
        "out.txt"
    );
    assert_eq!(git(&wt, &["branch", "--show-current"]), "furmi/happy");
}

#[test]
fn a_failing_task_is_retried_then_archived_as_failed() {
    let env = Env::new("");
    env.add("flaky", "echo boom >&2; exit 3", &["--retries", "1"]);

    assert_eq!(env.run_until_archived("flaky"), "failed");
    assert_eq!(env.field("flaky", "attempts").as_deref(), Some("2"));
    assert_eq!(env.field("flaky", "exit_code").as_deref(), Some("3"));
    assert!(env.field("flaky", "error").unwrap().contains("status 3"));
    for attempt in [1, 2] {
        let log =
            std::fs::read_to_string(env.home.path().join(format!("logs/flaky.{attempt}.log")))
                .unwrap();
        assert!(log.contains("boom"), "stderr is logged too: {log}");
    }
}

#[test]
fn a_timeout_stops_the_agent_and_everything_it_started() {
    let env = Env::new("");
    env.add(
        "slow",
        "sleep 300 & echo $! > pid.txt; wait",
        &["--timeout", "2s", "--retries", "0"],
    );

    assert_eq!(env.run_until_archived("slow"), "failed");
    assert!(env.field("slow", "error").unwrap().contains("timed out"));
    let pid = std::fs::read_to_string(env.worktree("slow").join("pid.txt")).unwrap();
    wait_for("the sleep to be killed", Duration::from_secs(10), || {
        !alive(pid.trim())
    });
}

#[test]
fn losing_the_tmux_server_requeues_without_using_a_retry() {
    let env = Env::new("");
    env.add("survivor", "sleep 300", &["--retries", "0"]);

    env.tick();
    assert_eq!(env.field("survivor", "status").as_deref(), Some("running"));
    env.tmux_kill_server(); // like a reboot

    env.tick();
    assert_eq!(env.field("survivor", "status").as_deref(), Some("running"));
    assert_eq!(env.field("survivor", "interruptions").as_deref(), Some("1"));
    assert_eq!(env.field("survivor", "attempts").as_deref(), Some("2"));
    assert!(
        !env.archived("survivor"),
        "retries: 0, yet it wasn't failed"
    );

    env.furmi_ok(&["cancel", "survivor"]);
}

#[test]
fn only_one_runner_per_home_and_status_notices() {
    let env = Env::new("");
    let status =
        |env: &Env| String::from_utf8_lossy(&env.furmi_ok(&["status"]).stdout).into_owned();
    assert!(status(&env).contains("NOT running"));

    let mut runner = env
        .furmi()
        .arg("run")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("the runner to show up", Duration::from_secs(10), || {
        status(&env).contains("runner     running")
    });

    let second = env.furmi().args(["run", "--once"]).output().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));

    runner.kill().unwrap();
    runner.wait().unwrap();
    assert!(
        status(&env).contains("NOT running"),
        "the claim dies with the runner"
    );
}

#[test]
fn doctor_checks_the_setup() {
    let env = Env::new("");
    let out = env.furmi().arg("doctor").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("ok    tmux is installed"), "{text}");
    assert!(text.contains("agent `stub`"), "{text}");

    std::fs::write(env.home.path().join("tasks/bad.md"), "garbage").unwrap();
    let out = env.furmi().arg("doctor").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("bad.md"));
}

/// Schedule file whose last firing was long ago, so the next pass enqueues.
fn due_schedule(env: &Env, extra: &str) {
    let long_ago = chrono::Local::now().fixed_offset() - chrono::Duration::hours(3);
    std::fs::write(
        env.home.path().join("schedules/tick.md"),
        format!(
            "---\nid: tick\ncron: \"* * * * *\"\nrepo: {}\nretries: 0\n{extra}\
             last_enqueued_at: {}\n---\n\nsleep 300\n",
            env.repo.path().display(),
            long_ago.to_rfc3339()
        ),
    )
    .unwrap();
}

fn scheduled_tasks(env: &Env, schedule: &str) -> usize {
    std::fs::read_dir(env.home.path().join("tasks"))
        .unwrap()
        .filter_map(|e| std::fs::read_to_string(e.unwrap().path()).ok())
        .filter(|t| t.contains(&format!("created_by: schedule:{schedule}")))
        .count()
}

#[test]
fn a_schedule_does_not_pile_up_runs_unless_allowed() {
    let env = Env::new("");
    due_schedule(&env, "");
    env.tick();
    assert_eq!(scheduled_tasks(&env, "tick"), 1);

    due_schedule(&env, ""); // another slot passed; the first run is still going
    env.tick();
    assert_eq!(
        scheduled_tasks(&env, "tick"),
        1,
        "skipped while the last run is queued"
    );

    due_schedule(&env, "allow_overlap: true\n");
    env.tick();
    assert_eq!(scheduled_tasks(&env, "tick"), 2);
}

#[test]
fn the_daily_budget_stops_new_tasks_from_starting() {
    let env = Env::new("daily_budget_usd = 1.0");
    std::fs::write(
        env.home.path().join("archive/spent.md"),
        format!(
            "---\nid: spent\nstatus: done\nrepo: {}\nfinished_at: {}\ncost_usd: 5.0\n---\n\nx\n",
            env.repo.path().display(),
            chrono::Local::now().to_rfc3339()
        ),
    )
    .unwrap();
    env.add("waiting", "true", &[]);

    env.tick();
    assert_eq!(env.field("waiting", "status").as_deref(), Some("pending"));
    let status = String::from_utf8_lossy(&env.furmi_ok(&["status"]).stdout).into_owned();
    assert!(status.contains("$5.0000 of $1.0000"), "{status}");
}

#[test]
fn a_slow_on_finish_does_not_block_other_commands() {
    let env = Env::new("");
    // A bare "origin" whose pre-push hook takes a while stands in for a slow network.
    let origin = tempfile::tempdir().unwrap();
    git(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
    let repo = env.repo.path();
    git(
        repo,
        &[
            "remote",
            "add",
            "origin",
            &origin.path().display().to_string(),
        ],
    );
    let hook = repo.join(".git/hooks/pre-push");
    std::fs::write(&hook, "#!/bin/sh\nsleep 8\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    env.add("pushy", "echo work > f.txt", &["--on-finish", "push"]);
    env.add("bystander", "true", &["--in", "10h"]);

    env.tick(); // starts it
    wait_for("the agent to exit", Duration::from_secs(30), || {
        env.home.path().join("logs/pushy.1.exit").exists()
    });
    let mut finishing = env.furmi().args(["run", "--once"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1500)); // now inside the hook

    let started = Instant::now();
    env.furmi_ok(&["cancel", "bystander"]); // needs the home lock
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "blocked behind on_finish"
    );
    assert!(
        finishing.try_wait().unwrap().is_none(),
        "on_finish still running"
    );

    assert!(finishing.wait().unwrap().success());
    assert_eq!(env.field("pushy", "status").as_deref(), Some("done"));
    assert_eq!(
        git(origin.path(), &["branch", "--list", "furmi/pushy"]).trim(),
        "furmi/pushy"
    );
}

/// `furmi done` runs `on_finish` (here: a push whose pre-push hook
/// takes a while) without holding the home lock, so the runner,
/// the dashboard and other commands aren't stalled behind it.
#[test]
fn done_does_not_hold_the_home_lock_while_on_finish_runs() {
    let env = Env::new("");
    // A bare "origin" whose pre-push hook takes a while stands in
    // for a slow network.
    let origin = tempfile::tempdir().unwrap();
    git(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
    let repo = env.repo.path();
    git(
        repo,
        &[
            "remote",
            "add",
            "origin",
            &origin.path().display().to_string(),
        ],
    );
    let hook = repo.join(".git/hooks/pre-push");
    std::fs::write(&hook, "#!/bin/sh\nsleep 8\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // The agent outlives the tick that starts it, so the task is
    // still `running` afterwards and `furmi done` — not the runner —
    // runs its on_finish.
    env.add(
        "pushy",
        "sleep 2; echo work > f.txt",
        &["--on-finish", "push"],
    );
    env.add("bystander", "true", &["--in", "10h"]);

    env.tick(); // starts it
    wait_for("the agent to exit", Duration::from_secs(30), || {
        env.home.path().join("logs/pushy.1.exit").exists()
    });
    let mut finishing = env.furmi().args(["done", "pushy"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1500)); // now inside the hook

    let started = Instant::now();
    env.furmi_ok(&["cancel", "bystander"]); // needs the home lock
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "blocked behind on_finish"
    );
    assert!(
        finishing.try_wait().unwrap().is_none(),
        "on_finish still running"
    );

    // The window is gone but the task isn't done yet: a runner pass now
    // must not mistake it for an interrupted run and requeue it.
    env.tick();
    assert_eq!(
        env.field("pushy", "status").as_deref(),
        Some("needs_review")
    );
    assert_eq!(env.field("pushy", "attempts").as_deref(), Some("1"));

    assert!(finishing.wait().unwrap().success());
    assert_eq!(env.field("pushy", "status").as_deref(), Some("done"));
    assert_eq!(
        git(origin.path(), &["branch", "--list", "furmi/pushy"]).trim(),
        "furmi/pushy"
    );
}

/// `furmi done` from inside the task's window defers the window kill
/// until after `on_finish`; it must still happen when `on_finish`
/// fails, so the window doesn't outlive the attempt.
#[test]
fn done_kills_the_window_even_when_on_finish_fails() {
    let env = Env::new("");
    // No "origin": the push in on_finish fails.
    env.add(
        "pushy",
        "sleep 2; echo work > f.txt",
        &["--on-finish", "push"],
    );

    env.tick(); // starts it
    wait_for("the agent to exit", Duration::from_secs(30), || {
        env.home.path().join("logs/pushy.1.exit").exists()
    });
    // FURMI_TASK_ID makes `furmi done` believe it runs inside the
    // task's window, so the kill waits until on_finish is over —
    // and on_finish (the push) fails.
    let out = env
        .furmi()
        .env("FURMI_TASK_ID", "pushy")
        .args(["done", "pushy"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(env.field("pushy", "status").as_deref(), Some("failed"));

    // The window was killed even though on_finish failed.
    let windows = Command::new("tmux")
        .args(["list-windows", "-t", "=furmit", "-F", "#{window_name}"])
        .env("TMUX_TMPDIR", env.tmux.path())
        .env_remove("TMUX")
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&windows.stdout);
    assert!(
        !names.lines().any(|w| w == "pushy"),
        "the task's window outlived its failed on_finish: {names}"
    );
}

/// What the fake `gh pr view` reports for the current branch.
enum FakePr {
    /// No PR for the branch (`gh pr view` fails).
    None,
    /// An open PR whose head is the branch's current commit.
    AtHead,
    /// An open PR for an older commit: a stale `furmi/<id>`
    /// branch from an earlier task.
    AtOlderCommit,
    /// A merged PR: closed, so it no longer covers the branch.
    Merged,
}

/// A fake `gh` that answers `pr view` as `pr` says and records
/// `pr create` calls. Returns the directory to put first on `$PATH`.
fn fake_gh(dir: &Path, pr: FakePr) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    // `pr view` prints `<state> <headRefOid>` — the shape `gh
    // pr view --json state,headRefOid --jq ...` prints — or fails
    // when the branch has no PR at all.
    let view = match pr {
        FakePr::None => "exit 1".to_string(),
        FakePr::AtHead => r#"echo "OPEN $(git rev-parse HEAD)""#.to_string(),
        FakePr::AtOlderCommit => {
            r##"echo "OPEN 0000000000000000000000000000000000000000""##.to_string()
        }
        FakePr::Merged => r#"echo "MERGED $(git rev-parse HEAD)""#.to_string(),
    };
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\ncase \"$2\" in\n  view) {view};;\n  create) echo created >> \"{}\";;\nesac\n",
            dir.join("gh-creates.log").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn finish_with_pr(pr: FakePr) -> usize {
    let env = Env::new("");
    let origin = tempfile::tempdir().unwrap();
    git(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
    git(
        env.repo.path(),
        &[
            "remote",
            "add",
            "origin",
            &origin.path().display().to_string(),
        ],
    );
    let bin = fake_gh(env.home.path(), pr);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    env.add("prtask", "echo work > f.txt", &["--on-finish", "pr"]);

    wait_for("prtask to be archived", Duration::from_secs(60), || {
        ok(env
            .furmi()
            .env("PATH", &path)
            .args(["run", "--once"])
            .output()
            .unwrap());
        env.archived("prtask")
    });
    assert_eq!(env.field("prtask", "status").as_deref(), Some("done"));
    std::fs::read_to_string(env.home.path().join("gh-creates.log"))
        .map(|t| t.lines().count())
        .unwrap_or(0)
}

#[test]
fn on_finish_pr_opens_a_pr_when_there_is_none() {
    assert_eq!(finish_with_pr(FakePr::None), 1);
}

#[test]
fn on_finish_pr_does_not_open_a_second_pr_for_the_branch() {
    assert_eq!(finish_with_pr(FakePr::AtHead), 0);
}

/// A stale `furmi/<id>` branch from an earlier task can have an open
/// PR of its own, pointing at an older commit: that doesn't cover
/// this run's work, so a PR is still opened.
#[test]
fn on_finish_pr_opens_a_pr_for_a_branch_pr_at_an_older_commit() {
    assert_eq!(finish_with_pr(FakePr::AtOlderCommit), 1);
}

#[test]
fn on_finish_pr_opens_a_pr_when_the_branch_pr_was_merged() {
    assert_eq!(finish_with_pr(FakePr::Merged), 1);
}

/// A fake `tmux` that fails its first call with a transient error and
/// delegates to the real one afterwards. Returns the directory to put
/// first on `$PATH`.
fn flaky_tmux(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let real = Command::new("sh")
        .args(["-c", "command -v tmux"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap();
    assert!(!real.is_empty(), "tmux is not on $PATH");
    let tmux = bin.join("tmux");
    std::fs::write(
        &tmux,
        format!(
            "#!/bin/sh\nmarker='{}'\nif [ ! -f \"$marker\" ]; then\n  : > \"$marker\"\n  \
             echo 'tmux: transient failure' >&2\n  exit 1\nfi\nexec '{real}' \"$@\"\n",
            dir.join("flaky-tmux-once").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// A tmux hiccup (a transient error, as opposed to the session being
/// genuinely gone) must not be mistaken for a lost window: the healthy
/// running task is left alone and the pass still succeeds.
#[test]
fn a_transient_tmux_error_does_not_requeue_a_running_task() {
    let env = Env::new("");
    env.add("steady", "sleep 300", &["--retries", "0"]);

    env.tick();
    assert_eq!(env.field("steady", "status").as_deref(), Some("running"));
    assert_eq!(env.field("steady", "attempts").as_deref(), Some("1"));

    let bin = flaky_tmux(env.home.path());
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    ok(env
        .furmi()
        .env("PATH", &path)
        .args(["run", "--once"])
        .output()
        .unwrap());

    assert_eq!(env.field("steady", "status").as_deref(), Some("running"));
    assert_eq!(env.field("steady", "attempts").as_deref(), Some("1"));
    assert_eq!(env.field("steady", "interruptions"), None);
    assert!(!env.archived("steady"), "the healthy task was torn down");

    let log = std::fs::read_to_string(env.home.path().join("runner.log")).unwrap();
    assert!(log.contains("transient failure"), "{log}");

    // The tmux error cleared: the window is still there and the queue
    // goes on serving.
    env.tick();
    assert_eq!(env.field("steady", "status").as_deref(), Some("running"));
    assert_eq!(env.field("steady", "attempts").as_deref(), Some("1"));

    env.furmi_ok(&["cancel", "steady"]);
}

// --- crash recovery ------------------------------------------------------------

/// `furmi run` killed outright (SIGKILL: no cleanup, no signal
/// handlers) while a task is running, then started again. The
/// tmux server outlives the runner, so the agent keeps going:
/// the new runner must let it finish, not start it again, and
/// the task must end archived, not stuck `running`.
#[test]
fn a_killed_runner_leaves_no_task_stuck_running_or_running_twice() {
    let env = Env::new("");
    env.add(
        "rescue",
        "echo run >> runs.txt; sleep 5",
        &["--retries", "0"],
    );

    let mut runner = env.runner();
    wait_for("the task to start", Duration::from_secs(30), || {
        env.field("rescue", "status").as_deref() == Some("running")
    });

    runner.kill().unwrap(); // SIGKILL, like an OOM kill or a crash
    runner.wait().unwrap();

    // A fresh process takes over the claim (the lock dies with
    // the process) and picks the task back up.
    let mut runner = env.runner();
    wait_for("rescue to be archived", Duration::from_secs(60), || {
        env.archived("rescue")
    });
    runner.kill().unwrap();
    runner.wait().unwrap();

    assert_eq!(env.field("rescue", "status").as_deref(), Some("done"));
    assert_eq!(
        env.field("rescue", "attempts").as_deref(),
        Some("1"),
        "the attempt must not be restarted"
    );
    let runs = std::fs::read_to_string(env.worktree("rescue").join("runs.txt")).unwrap();
    assert_eq!(
        runs.lines().count(),
        1,
        "the agent ran exactly once: {runs:?}"
    );
}

/// A running `furmi run` asked to stop (SIGTERM, as `furmi stop` does) must not
/// touch the task: it exits cleanly, the agent keeps going in its tmux
/// window, and a fresh runner finishes it without a second attempt.
#[test]
fn sigterm_leaves_a_running_task_for_the_next_runner() {
    let env = Env::new("");
    env.add(
        "longrun",
        "echo ran >> runs.txt; sleep 5",
        &["--retries", "0"],
    );

    let mut runner = env.runner();
    wait_for("the task to start", Duration::from_secs(30), || {
        env.field("longrun", "status").as_deref() == Some("running")
    });

    let pid = i32::try_from(runner.id()).unwrap();
    // SAFETY: plain kill(2) on the child we spawned.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    wait_for("the runner to exit", Duration::from_secs(10), || {
        runner.try_wait().unwrap().is_some()
    });
    let status = runner.wait().unwrap();
    assert!(status.success(), "the runner exited badly: {status:?}");

    // Untouched: still running, still the first attempt.
    assert_eq!(env.field("longrun", "status").as_deref(), Some("running"));
    assert_eq!(env.field("longrun", "attempts").as_deref(), Some("1"));

    // A new runner finds the live window and lets it finish.
    let mut runner = env.runner();
    wait_for("longrun to be archived", Duration::from_secs(60), || {
        env.archived("longrun")
    });
    runner.kill().unwrap();
    runner.wait().unwrap();

    assert_eq!(env.field("longrun", "status").as_deref(), Some("done"));
    assert_eq!(env.field("longrun", "attempts").as_deref(), Some("1"));
    let runs = std::fs::read_to_string(env.worktree("longrun").join("runs.txt")).unwrap();
    assert_eq!(runs.lines().count(), 1, "the agent ran once: {runs:?}");
}

/// A running attempt whose exit record is unreadable (hand-edited
/// or corrupted) must not fail the pass: the task stays running
/// while its window lives, and is requeued as an interruption once
/// the window is gone.
#[test]
fn a_corrupt_exit_record_is_ignored_until_the_attempt_ends() {
    let env = Env::new("");
    env.add("stubborn", "sleep 300", &["--retries", "0"]);

    env.tick();
    assert_eq!(env.field("stubborn", "status").as_deref(), Some("running"));

    std::fs::write(env.home.path().join("logs/stubborn.1.exit"), "garbage").unwrap();
    env.tick(); // must not fail on the record
    assert_eq!(env.field("stubborn", "status").as_deref(), Some("running"));
    assert_eq!(env.field("stubborn", "attempts").as_deref(), Some("1"));

    env.tmux_kill_server(); // the attempt is cut short
    env.tick();
    assert_eq!(
        env.field("stubborn", "interruptions").as_deref(),
        Some("1"),
        "a lost window is an interruption, not a failure"
    );

    env.furmi_ok(&["cancel", "stubborn"]);
}

/// Two `furmi run` processes on one home: the second must refuse,
/// and the first keeps serving.
#[test]
fn a_second_runner_process_refuses_to_start() {
    let env = Env::new("");
    let status =
        |env: &Env| String::from_utf8_lossy(&env.furmi_ok(&["status"]).stdout).into_owned();

    let mut runner = env.runner();
    wait_for("the runner to show up", Duration::from_secs(10), || {
        status(&env).contains("runner     running")
    });

    let second = env.furmi().arg("run").output().unwrap();
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // The first runner is unaffected, and still runs the queue.
    env.add("served", "true", &[]);
    wait_for("served to be archived", Duration::from_secs(60), || {
        env.archived("served")
    });
    assert_eq!(env.field("served", "status").as_deref(), Some("done"));

    runner.kill().unwrap();
    runner.wait().unwrap();
}

/// Task and schedule files written non-atomically (or edited by
/// hand) so their frontmatter is half-written: the runner skips
/// and reports them, keeps them on disk, and still runs everything
/// else.
#[test]
fn half_written_task_and_schedule_files_are_skipped_and_reported() {
    let env = Env::new("");
    // No closing frontmatter, mid-keyword: what a crash or an
    // interrupted save leaves behind.
    std::fs::write(
        env.home.path().join("tasks/half-task.md"),
        "---\nid: half-task\nrepo: ",
    )
    .unwrap();
    std::fs::write(
        env.home.path().join("schedules/half-schedule.md"),
        "---\nid: half-schedule\ncron: ",
    )
    .unwrap();
    // Complete frontmatter, but a field that isn't allowed.
    std::fs::write(
        env.home.path().join("tasks/typo.md"),
        format!(
            "---\nid: typo\nrepo: {}\nretrs: 1\n---\ntrue\n",
            env.repo.path().display()
        ),
    )
    .unwrap();
    env.add("healthy", "true", &[]);

    // Each pass is a fresh process, and none of them may crash,
    // hang or fail because of the broken files.
    wait_for("healthy to be archived", Duration::from_secs(60), || {
        env.tick();
        env.archived("healthy")
    });
    assert_eq!(env.field("healthy", "status").as_deref(), Some("done"));

    for name in [
        "tasks/half-task.md",
        "schedules/half-schedule.md",
        "tasks/typo.md",
    ] {
        assert!(env.home.path().join(name).exists(), "{name} was dropped");
    }
    let log = std::fs::read_to_string(env.home.path().join("runner.log")).unwrap();
    for name in ["half-task", "half-schedule", "typo"] {
        assert!(log.contains(name), "{name} was not reported:\n{log}");
    }
    assert!(!env.archived("half-task"));
    assert!(!env.archived("typo"));
}

/// The tmux server survives, but the session the tasks run in is
/// gone (killed by hand, or renamed away): the running tasks are
/// requeued as interruptions, not left stuck `running`.
#[test]
fn losing_the_furmi_session_requeues_the_task() {
    let env = Env::new("");
    env.add("orphaned", "sleep 300", &["--retries", "0"]);

    env.tick();
    assert_eq!(env.field("orphaned", "status").as_deref(), Some("running"));

    env.tmux_kill_session();
    env.tick();
    assert_eq!(env.field("orphaned", "status").as_deref(), Some("running"));
    assert_eq!(
        env.field("orphaned", "interruptions").as_deref(),
        Some("1"),
        "a lost session is an interruption"
    );
    assert_eq!(env.field("orphaned", "attempts").as_deref(), Some("2"));
    assert!(
        !env.archived("orphaned"),
        "retries: 0, yet it wasn't failed"
    );

    env.furmi_ok(&["cancel", "orphaned"]);
}

/// The `last_enqueued_at` slot of a schedule, for slot tests.
fn last_enqueued(env: &Env, id: &str) -> Option<String> {
    let text = std::fs::read_to_string(env.home.path().join(format!("schedules/{id}.md"))).ok()?;
    text.lines()
        .skip(1)
        .take_while(|l| *l != "---")
        .find_map(|l| l.strip_prefix("last_enqueued_at: ").map(str::to_string))
}

/// A schedule whose slot is due must fire exactly once for it,
/// even across restarts in the same minute: `last_enqueued_at` is
/// recorded (atomically) before the task is created, so a fresh
/// `furmi run --once` in the same minute finds the slot already
/// taken.
#[test]
fn a_schedule_does_not_fire_twice_for_the_same_slot_after_a_restart() {
    use chrono::SubsecRound;

    let env = Env::new("");
    // Due: one minute ago, truncated to the second.
    let due = (chrono::Local::now().fixed_offset() - chrono::Duration::minutes(1)).trunc_subsecs(0);
    std::fs::write(
        env.home.path().join("schedules/minute.md"),
        format!(
            "---\nid: minute\ncron: \"* * * * *\"\nrepo: {}\nretries: 0\n\
             last_enqueued_at: {}\n---\n\nsleep 300\n",
            env.repo.path().display(),
            due.to_rfc3339()
        ),
    )
    .unwrap();

    // Each `run --once` is its own process: a restart.
    env.tick();
    assert_eq!(
        scheduled_tasks(&env, "minute"),
        1,
        "the due slot fires once"
    );
    let first_slot = last_enqueued(&env, "minute");

    env.tick();
    let second_slot = last_enqueued(&env, "minute");
    if second_slot == first_slot {
        assert_eq!(
            scheduled_tasks(&env, "minute"),
            1,
            "the same slot must not fire twice across restarts"
        );
    } else {
        // The minute rolled over between the two runs: the new
        // slot fires, exactly once.
        assert_eq!(
            scheduled_tasks(&env, "minute"),
            2,
            "a new minute fires exactly once"
        );
    }
}

/// A fake `gh` that is not authenticated: neither `pr view` nor `pr create`
/// works. Returns the directory to put first on `$PATH`.
fn fake_gh_unauthenticated(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\nif [ \"$2\" = view ]; then exit 1; fi\n\
         echo 'To get started with GitHub CLI, please run:  gh auth login' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[test]
fn on_finish_pr_failure_keeps_the_work_and_says_how_to_finish_it() {
    let env = Env::new("");
    let origin = tempfile::tempdir().unwrap();
    git(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
    git(
        env.repo.path(),
        &[
            "remote",
            "add",
            "origin",
            &origin.path().display().to_string(),
        ],
    );
    let bin = fake_gh_unauthenticated(env.home.path());
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    env.add(
        "prfail",
        "echo work > f.txt",
        &["--on-finish", "pr", "--retries", "0"],
    );

    wait_for("prfail to be archived", Duration::from_secs(60), || {
        ok(env
            .furmi()
            .env("PATH", &path)
            .args(["run", "--once"])
            .output()
            .unwrap());
        env.archived("prfail")
    });
    assert_eq!(env.field("prfail", "status").as_deref(), Some("failed"));
    let error = std::fs::read_to_string(env.home.path().join("archive/prfail.md")).unwrap();
    assert!(error.contains("on_finish failed"), "{error}");
    assert!(error.contains("gh auth login"), "{error}");
    assert!(error.contains("furmi/prfail"), "{error}");
    assert!(error.contains("furmi retry prfail"), "{error}");

    // The commit `on_finish` made before `gh` failed is still there.
    let wt = env.worktree("prfail");
    assert_eq!(git(&wt, &["branch", "--show-current"]), "furmi/prfail");
    assert_eq!(
        git(&wt, &["show", "--name-only", "--format=", "HEAD"]),
        "f.txt"
    );
    assert!(
        !git(env.repo.path(), &["branch", "--list", "furmi/prfail"])
            .trim()
            .is_empty(),
        "the branch is kept"
    );
}

#[test]
fn a_pane_that_dies_without_a_status_fails_with_an_explanation() {
    let env = Env::new("");
    // `$PPID` here is `furmi exec`: killing it leaves a dead pane but no
    // `logs/<id>.<attempt>.exit`, which is the production failure this
    // reproduces.
    env.add("selfkill", "kill -9 $PPID", &["--retries", "0"]);

    assert_eq!(env.run_until_archived("selfkill"), "failed");
    assert_eq!(env.field("selfkill", "attempts").as_deref(), Some("6"));
    assert_eq!(env.field("selfkill", "interruptions").as_deref(), Some("5"));
    let error = env.field("selfkill", "error").unwrap();
    assert!(
        error.contains("without recording an exit status"),
        "{error}"
    );
    assert!(error.contains("interrupted 5 times"), "{error}");

    for attempt in 1..=6 {
        assert!(
            !env.home
                .path()
                .join(format!("logs/selfkill.{attempt}.exit"))
                .exists(),
            "attempt {attempt} had no `.exit`, as in the production case"
        );
        let log =
            std::fs::read_to_string(env.home.path().join(format!("logs/selfkill.{attempt}.log")))
                .unwrap();
        assert!(log.contains("attempt interrupted"), "{log}");
    }
}

#[test]
fn a_rate_limit_error_is_retried() {
    let env = Env::new("");
    // The first attempt hits a rate limit; the retry succeeds.
    env.add(
        "limited",
        "if [ -e ran-once ]; then exit 0; fi; touch ran-once; \
         echo 'Error from provider (Console): Rate limit exceeded. Please try again later.' >&2; \
         exit 1",
        &["--retries", "1", "--no-worktree"],
    );

    assert_eq!(env.run_until_archived("limited"), "done");
    assert_eq!(
        env.field("limited", "attempts").as_deref(),
        Some("2"),
        "a rate limit is worth another attempt"
    );
}

#[test]
fn a_provider_error_fails_with_its_reason_and_is_not_retried() {
    let env = Env::new("");
    env.add(
        "provider",
        "printf '\\033[91m\\033[1mError: \\033[0mError from provider (Console): \
         This model is not available in your country\\n' >&2; exit 1",
        &["--retries", "2"],
    );

    assert_eq!(env.run_until_archived("provider"), "failed");
    assert_eq!(
        env.field("provider", "attempts").as_deref(),
        Some("1"),
        "the identical model was not retried"
    );
    assert_eq!(env.field("provider", "exit_code").as_deref(), Some("1"));
    let error = env.field("provider", "error").unwrap();
    assert!(error.contains("not available in your country"), "{error}");
    assert!(error.contains("Retrying the same model"), "{error}");
    assert!(error.contains("then run `furmi retry provider`"), "{error}");
    assert!(
        !error.contains('\u{1b}'),
        "terminal colours were stripped: {error}"
    );
}

#[test]
fn a_successful_run_mentioning_a_provider_error_stays_done() {
    // An agent working on code that handles these errors may print the
    // phrase; only failed attempts are scanned for it.
    let env = Env::new("");
    env.add(
        "mentions",
        "echo 'handled: Error from provider, model not found'; exit 0",
        &[],
    );

    assert_eq!(env.run_until_archived("mentions"), "done");
}

#[test]
fn a_task_whose_agent_binary_is_missing_fails_before_starting() {
    let env = Env::new(
        "[agents.missingbin]\nheadless = [\"furmi-no-such-agent-binary\", \"{prompt}\"]\n\
         interactive = [\"furmi-no-such-agent-binary\", \"{prompt}\"]\n",
    );
    env.add(
        "nobin",
        "do work",
        &["--agent", "missingbin", "--retries", "5"],
    );

    assert_eq!(env.run_until_archived("nobin"), "failed");
    assert_eq!(
        env.field("nobin", "attempts"),
        None,
        "no attempt was made, so no retry either"
    );
    let error = env.field("nobin", "error").unwrap();
    assert!(error.contains("furmi-no-such-agent-binary"), "{error}");
    assert!(error.contains("not on $PATH"), "{error}");
    assert!(error.contains("install it"), "{error}");
}

#[test]
fn a_task_with_an_unconfigured_agent_fails_with_an_explanation() {
    let env = Env::new("");
    // `furmi add` rejects this, but a task written by hand, an agent or a
    // schedule can still name an agent that has no [agents.*] section.
    std::fs::write(
        env.home.path().join("tasks/ghost.md"),
        format!(
            "---\nid: ghost\nrepo: {}\nagent: nosuchagent\n---\n\nwork\n",
            env.repo.path().display()
        ),
    )
    .unwrap();

    assert_eq!(env.run_until_archived("ghost"), "failed");
    let error = env.field("ghost", "error").unwrap();
    assert!(error.contains("unknown agent `nosuchagent`"), "{error}");
    assert!(error.contains("[agents.nosuchagent]"), "{error}");
}
