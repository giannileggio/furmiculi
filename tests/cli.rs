//! End-to-end tests against the built `furmi` binary.
//!
//! The unit tests under `src/` cover each command's logic directly; these
//! instead go through `Cli::parse` and the real filesystem, to catch
//! problems in argument wiring (flags, conflicts, global `--home`) and in
//! how commands compose through the process boundary (exit codes, stdin,
//! files one command leaves for the next to read).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A fresh `furmi` invocation against `home`. `$HOME` is also pointed at it, so
/// the global-skill links `furmi init` may touch land inside the temp dir
/// instead of the real user home.
fn furmi(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_furmi"));
    cmd.env("HOME", home)
        .arg("--home")
        .arg(home)
        .stdin(Stdio::null());
    cmd
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("spawning furmi")
}

fn ok(out: Output) -> Output {
    assert!(
        out.status.success(),
        "expected success, got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        stdout(&out),
        stderr(&out)
    );
    out
}

fn failed(out: Output) -> Output {
    assert!(
        !out.status.success(),
        "expected failure, got success\nstdout: {}",
        stdout(&out)
    );
    out
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn init(home: &Path) {
    ok(run(furmi(home).args([
        "init",
        "--agent",
        "claude",
        "--no-global-skill",
    ])));
}

fn add(home: &Path, repo: &Path, id: &str, prompt: &str) {
    ok(run(furmi(home).args([
        "add",
        "--repo",
        &repo.display().to_string(),
        "--no-worktree",
        "--id",
        id,
        "--prompt",
        prompt,
    ])));
}

#[test]
fn init_creates_the_home_layout() {
    let home = tempfile::tempdir().unwrap();

    let out = ok(run(furmi(home.path()).args([
        "init",
        "--agent",
        "claude",
        "--no-global-skill",
    ])));
    assert!(stdout(&out).contains("wrote"));
    assert!(stdout(&out).contains("initialized"));
    assert!(home.path().join("config.toml").is_file());
    assert!(home.path().join("AGENTS.md").is_file());
    for dir in ["tasks", "schedules", "archive", "logs", "worktrees"] {
        assert!(home.path().join(dir).is_dir(), "missing {dir}/");
    }

    // Re-running keeps the user's config instead of overwriting it.
    let out = ok(run(furmi(home.path()).args([
        "init",
        "--agent",
        "claude",
        "--no-global-skill",
    ])));
    assert!(stdout(&out).contains("kept existing"));
}

#[test]
fn add_ls_and_validate_roundtrip() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());

    let out = ok(run(furmi(home.path()).args([
        "add",
        "--repo",
        &repo.path().display().to_string(),
        "--no-worktree",
        "--id",
        "my-task",
        "--prompt",
        "do the thing",
    ])));
    assert!(stdout(&out).trim_end().ends_with("my-task.md"));

    let out = ok(run(furmi(home.path()).arg("ls")));
    let listing = stdout(&out);
    assert!(listing.contains("my-task"));
    assert!(listing.contains("pending"));

    let out = ok(run(furmi(home.path()).arg("validate")));
    let report = stdout(&out);
    assert!(report.contains("ok"));
    assert!(report.contains("1 file(s) checked, 0 with errors"));
}

#[test]
fn add_reads_the_prompt_from_stdin() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());

    let mut child = furmi(home.path())
        .args([
            "add",
            "--repo",
            &repo.path().display().to_string(),
            "--no-worktree",
            "--id",
            "from-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"  fix the thing  \n")
        .unwrap();
    let out = ok(child.wait_with_output().unwrap());
    assert!(stdout(&out).trim_end().ends_with("from-stdin.md"));

    let body = std::fs::read_to_string(home.path().join("tasks/from-stdin.md")).unwrap();
    assert!(body.trim_end().ends_with("fix the thing"));
}

#[test]
fn add_rejects_an_empty_prompt() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());

    let out = failed(run(furmi(home.path()).args([
        "add",
        "--repo",
        &repo.path().display().to_string(),
        "--no-worktree",
        "--id",
        "empty",
    ])));
    assert!(stderr(&out).contains("prompt is empty"));
}

#[test]
fn add_rejects_a_missing_repo() {
    let home = tempfile::tempdir().unwrap();
    init(home.path());

    let out = failed(run(furmi(home.path()).args([
        "add",
        "--repo",
        "/no/such/repo",
        "--no-worktree",
        "--prompt",
        "x",
    ])));
    assert!(stderr(&out).contains("does not exist"));
}

#[test]
fn add_rejects_a_duplicate_id() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());
    add(home.path(), repo.path(), "dup", "first");

    let out = failed(run(furmi(home.path()).args([
        "add",
        "--repo",
        &repo.path().display().to_string(),
        "--no-worktree",
        "--id",
        "dup",
        "--prompt",
        "second",
    ])));
    assert!(stderr(&out).contains("already exists"));
}

#[test]
fn cancel_then_retry_via_cli() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());
    add(home.path(), repo.path(), "t1", "do it");

    ok(run(furmi(home.path()).args(["cancel", "t1"])));
    let archived = stdout(&ok(run(furmi(home.path()).args(["ls", "--archive"]))));
    assert!(archived.contains("t1") && archived.contains("cancelled"));
    let pending = stdout(&ok(run(furmi(home.path()).arg("ls"))));
    assert!(pending.contains("(none)"));

    ok(run(furmi(home.path()).args(["retry", "t1"])));
    let pending = stdout(&ok(run(furmi(home.path()).arg("ls"))));
    assert!(pending.contains("t1") && pending.contains("pending"));
    let archived = stdout(&ok(run(furmi(home.path()).args(["ls", "--archive"]))));
    assert!(archived.contains("(none)"));
}

#[test]
fn logs_reads_a_saved_attempt_via_the_cli() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());
    add(home.path(), repo.path(), "t1", "do it");

    let out = failed(run(furmi(home.path()).args(["logs", "t1"])));
    assert!(stderr(&out).contains("hasn't started"));

    // Fake a finished attempt: bump `attempts` on the task file and write
    // the log `furmi run` would have saved.
    let task_path = home.path().join("tasks/t1.md");
    let content = std::fs::read_to_string(&task_path).unwrap();
    std::fs::write(
        &task_path,
        content.replacen("---\n", "---\nattempts: 1\n", 1),
    )
    .unwrap();
    std::fs::write(home.path().join("logs/t1.1.log"), "hello from the agent\n").unwrap();

    let out = stdout(&ok(run(furmi(home.path()).args(["logs", "t1"]))));
    assert_eq!(out, "hello from the agent\n");

    let out = failed(run(furmi(home.path()).args(["logs", "t1", "--attempt", "2"])));
    assert!(stderr(&out).contains("1 attempt"));
}

#[test]
fn commands_that_need_existing_state_fail_clearly_before_init() {
    let home = tempfile::tempdir().unwrap();

    let out = failed(run(furmi(home.path()).args(["done", "some-task"])));
    assert!(stderr(&out).contains("not initialized"));
}

#[test]
fn first_use_sets_everything_up() {
    let home = tempfile::tempdir().unwrap();

    let out = ok(run(furmi(home.path()).arg("ls")));
    assert!(stderr(&out).contains("First run"));
    assert!(!stdout(&out).contains("First run"));
    assert!(home.path().join("config.toml").is_file());
    assert!(home.path().join("tasks").is_dir());

    // The second time it's just the listing.
    let out = ok(run(furmi(home.path()).arg("ls")));
    assert!(!stderr(&out).contains("First run"));
}

/// A throwaway git repository.
fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repo.path())
        .status()
        .expect("running git");
    assert!(status.success());
    repo
}

#[test]
fn add_needs_no_flags_inside_a_repo() {
    let home = tempfile::tempdir().unwrap();
    let repo = git_repo();
    init(home.path());

    let out = ok(run(furmi(home.path())
        .current_dir(repo.path())
        .args(["add", "Fix the login redirect"])));
    assert!(
        stdout(&out)
            .trim_end()
            .ends_with("fix-the-login-redirect.md")
    );

    let task =
        std::fs::read_to_string(home.path().join("tasks/fix-the-login-redirect.md")).unwrap();
    let repo_name = repo.path().file_name().unwrap().to_str().unwrap();
    assert!(task.contains(repo_name), "{task}");
    assert!(task.contains("Fix the login redirect"));
}

#[test]
fn add_outside_a_repo_says_what_to_do() {
    let home = tempfile::tempdir().unwrap();
    let not_a_repo = tempfile::tempdir().unwrap();
    init(home.path());

    let out = failed(run(furmi(home.path())
        .current_dir(not_a_repo.path())
        .args(["add", "Fix it"])));
    let err = stderr(&out);
    assert!(err.contains("isn't inside a git repository"), "{err}");
    assert!(err.contains("--repo"), "{err}");
}

#[test]
fn the_prompt_is_positional_or_a_flag_but_not_both() {
    let home = tempfile::tempdir().unwrap();
    let repo = git_repo();
    init(home.path());

    ok(run(furmi(home.path())
        .current_dir(repo.path())
        .args(["add", "--prompt", "via flag", "--id", "flagged"])));
    failed(run(furmi(home.path()).current_dir(repo.path()).args([
        "add",
        "positional",
        "--prompt",
        "and flag",
    ])));
}

#[test]
fn bare_furmi_explains_how_to_start() {
    let home = tempfile::tempdir().unwrap();

    let out = stdout(&ok(run(&mut furmi(home.path()))));
    assert!(out.contains("furmi add"), "{out}");
    assert!(out.contains("furmi start"), "{out}");
    // It only explains: nothing gets created.
    assert!(!home.path().join("tasks").exists());

    init(home.path());
    let out = stdout(&ok(run(&mut furmi(home.path()))));
    assert!(out.contains("runner"), "{out}");
    assert!(out.contains("furmi start"), "{out}");
}

#[test]
fn ls_watch_refreshes_until_killed() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());
    add(home.path(), repo.path(), "watched", "do it");

    let mut child = furmi(home.path())
        .args(["ls", "--watch", "--interval", "20ms"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();

    let text = stdout(&out);
    assert!(
        text.matches("watched").count() >= 2,
        "expected multiple refreshes, got:\n{text}"
    );
    assert!(text.contains("\x1B[2J\x1B[H"));
}

/// A raw HTTP request over TCP (no HTTP client dependency). `ip` is where to
/// connect; `host` is the Host header sent. Retries the connection until the
/// server is up, and panics if it never comes up within a few seconds.
fn http_at(
    ip: &str,
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> String {
    use std::io::Read as _;
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect((ip, port)) {
            Ok(mut stream) => {
                let mut req = format!(
                    "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    req.push_str(&format!("{k}: {v}\r\n"));
                }
                write!(stream, "{req}\r\n{body}").unwrap();
                let mut resp = String::new();
                stream.read_to_string(&mut resp).unwrap();
                return resp;
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(30)),
            Err(e) => panic!("could not connect to {ip}:{port}: {e}"),
        }
    }
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> String {
    http_at("127.0.0.1", port, method, path, host, headers, body)
}

fn http_get(port: u16, path: &str) -> String {
    http(port, "GET", path, "127.0.0.1", &[], "")
}

/// A POST as the dashboard page itself would send it.
fn http_post(port: u16, token: &str, path: &str, body: &str) -> String {
    http(
        port,
        "POST",
        path,
        &format!("127.0.0.1:{port}"),
        &[
            ("X-Furmi-Token", token),
            ("Origin", &format!("http://127.0.0.1:{port}")),
            ("Content-Type", "application/json"),
        ],
        body,
    )
}

/// The per-run token `furmi web` embeds in the dashboard page.
fn page_token(port: u16) -> String {
    let index = http_get(port, "/");
    let marker = "name=\"furmi-token\" content=\"";
    let start = index.find(marker).expect("token meta tag") + marker.len();
    index[start..start + 32].to_string()
}

#[test]
fn web_actions_mutate_tasks_through_the_api() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());
    add(home.path(), repo.path(), "w1", "do it");

    let port = 18338u16;
    let mut child = furmi(home.path())
        .args(["web", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let token = page_token(port); // also waits for the server to come up

    let cancel = http_post(port, &token, "/api/tasks/w1/cancel", "");
    assert!(cancel.starts_with("HTTP/1.1 200"), "{cancel}");
    assert!(cancel.contains("\"ok\":true"));

    let state = http_get(port, "/api/state");
    assert!(state.contains("\"cancelled\":1"));
    assert!(state.contains("\"pending\":0"));

    let retry = http_post(port, &token, "/api/tasks/w1/retry", "");
    assert!(retry.starts_with("HTTP/1.1 200"), "{retry}");
    let state = http_get(port, "/api/state");
    assert!(state.contains("\"pending\":1"));

    let bad = http_post(port, &token, "/api/tasks/w1/fail", "");
    assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
    assert!(bad.contains("\"error\""));

    let logs = http_get(port, "/api/tasks/w1/logs");
    assert!(logs.starts_with("HTTP/1.1 404"), "{logs}");
    assert!(logs.contains("hasn't started"));

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn web_refuses_cross_site_and_rebound_requests() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());
    add(home.path(), repo.path(), "w1", "do it");

    let port = 18339u16;
    let mut child = furmi(home.path())
        .args(["web", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let token = page_token(port);
    let path = "/api/tasks/w1/cancel";
    let own_host = format!("127.0.0.1:{port}");

    // A page on another site: no token it could have read.
    let forged = http(
        port,
        "POST",
        path,
        &own_host,
        &[("Origin", "http://evil.example")],
        "",
    );
    assert!(forged.starts_with("HTTP/1.1 403"), "{forged}");
    // Right token but a foreign Origin.
    let origin = http(
        port,
        "POST",
        path,
        &own_host,
        &[("X-Furmi-Token", &token), ("Origin", "http://evil.example")],
        "",
    );
    assert!(origin.starts_with("HTTP/1.1 403"), "{origin}");
    // Wrong token.
    let wrong = http(port, "POST", path, &own_host, &[("X-Furmi-Token", "nope")], "");
    assert!(wrong.starts_with("HTTP/1.1 403"), "{wrong}");
    // DNS rebinding: the browser sends the attacker's hostname.
    let rebound = http(port, "GET", "/api/state", "evil.example", &[], "");
    assert!(rebound.starts_with("HTTP/1.1 403"), "{rebound}");
    let rebound_page = http(port, "GET", "/", &format!("evil.example:{port}"), &[], "");
    assert!(rebound_page.starts_with("HTTP/1.1 403"), "{rebound_page}");
    assert!(!rebound_page.contains(&token));

    // None of that touched the task.
    let state = http_get(port, "/api/state");
    assert!(state.contains("\"pending\":1"), "{state}");
    // The real thing still works.
    let ok = http_post(port, &token, path, "");
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");

    child.kill().unwrap();
    child.wait().unwrap();
}

/// The raw HTTP helper talks to `127.0.0.1`; the remote-mode test binds the
/// real listener on `0.0.0.0` — an address that is *not* a "loopback name",
/// so `furmi web` runs it in serving-other-machines mode — and connects through
/// `127.0.0.1`, which every platform routes, so the test can exercise every
/// gate without a second machine. (`127.0.0.2`, an earlier attempt at a
/// local-but-unloopback address, is routed on Linux but not on macOS.)
const TEST_TOKEN: &str = "d3adb33fc0ffeecad3adb33fc0ffeeca";

/// Duplicated token-file plumbing has one home: write `TEST_TOKEN` to a
/// 0600 file a test can point `--token-file` at.
fn write_token_file(home: &Path) -> std::path::PathBuf {
    let token_file = home.join("web-token");
    let mut file = std::fs::File::create(&token_file).unwrap();
    writeln!(file, "{TEST_TOKEN}").unwrap();
    let mut perms = std::fs::metadata(&token_file).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o600);
    std::fs::set_permissions(&token_file, perms).unwrap();
    token_file
}

#[test]
fn web_refuses_a_non_loopback_listen_without_a_token() {
    let home = tempfile::tempdir().unwrap();

    // No token: it must not start, and it must say why.
    let mut child = furmi(home.path())
        .args(["web", "--no-open", "--listen", "127.0.0.2:18442"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "still running: the server should refuse to start"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(!status.success(), "should exit non-zero");
    let err = String::from_utf8_lossy(&child.wait_with_output().unwrap().stderr).into_owned();
    assert!(err.contains("--token-file"), "{err}");
    assert!(err.contains("token"), "{err}");

    // A token but no allowlist: same refusal, for the Host header's sake.
    let token_file = write_token_file(home.path());

    let out = run(furmi(home.path()).args([
        "web",
        "--no-open",
        "--listen",
        "127.0.0.2:18442",
        "--token-file",
        &token_file.display().to_string(),
    ]));
    let err = stderr(&out);
    assert!(!out.status.success(), "should exit non-zero");
    assert!(err.contains("--allowed-host"), "{err}");
}

#[test]
fn web_remote_mode_gates_every_route_on_the_bearer_token() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());
    add(home.path(), repo.path(), "w1", "do it");

    let token_file = write_token_file(home.path());

    let port = 18443u16;
    let mut child = furmi(home.path())
        .args([
            "web",
            "--no-open",
            "--listen",
            &format!("0.0.0.0:{port}"),
            "--token-file",
            &token_file.display().to_string(),
            // A bare entry (any port) and an entry with a port, so both
            // allowlist behaviors get a workout.
            "--allowed-host",
            "toolnet.example",
            "--allowed-host",
            &format!("tools.example:{port}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // First load: the token in the URL opens the page, and the page embeds it.
    let page = http_at(
        "127.0.0.1",
        port,
        "GET",
        &format!("/?token={TEST_TOKEN}"),
        "toolnet.example",
        &[],
        "",
    );
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains(TEST_TOKEN), "{page}");
    assert!(page.contains("furmi web"));
    // ...and the browser is handed a cookie of its own, so a reload of the
    // now-stripped URL (or a bookmark) gets the page instead of a 401.
    assert!(
        page.contains(&format!("Set-Cookie: furmi_token={TEST_TOKEN}")),
        "{page}"
    );
    assert!(page.contains("HttpOnly"), "{page}");
    assert!(page.contains("SameSite=Strict"), "{page}");
    let reloaded = http_at(
        "127.0.0.1",
        port,
        "GET",
        "/",
        "toolnet.example",
        &[("Cookie", &format!("other=1; furmi_token={TEST_TOKEN}"))],
        "",
    );
    assert!(reloaded.starts_with("HTTP/1.1 200"), "{reloaded}");
    let wrong_cookie = http_at(
        "127.0.0.1",
        port,
        "GET",
        "/",
        "toolnet.example",
        &[("Cookie", "furmi_token=not-the-token")],
        "",
    );
    assert!(wrong_cookie.starts_with("HTTP/1.1 401"), "{wrong_cookie}");

    // Without the bearer or a cookie: every route refuses, including one a
    // page visits less.
    for route in [
        "/",
        "/api/state",
        "/api/doctor",
        "/api/agents",
        "/api/tasks/w1",
    ] {
        let denied = http_at("127.0.0.1", port, "GET", route, "toolnet.example", &[], "");
        assert!(denied.starts_with("HTTP/1.1 401"), "{route}: {denied}");
        assert!(!denied.contains(TEST_TOKEN));
    }
    let denied_post = http_at(
        "127.0.0.1",
        port,
        "POST",
        "/api/tasks/w1/cancel",
        "toolnet.example",
        &[
            ("X-Furmi-Token", TEST_TOKEN),
            ("Origin", "http://toolnet.example"),
        ],
        "",
    );
    assert!(denied_post.starts_with("HTTP/1.1 401"), "{denied_post}");

    // A wrong bearer is just as dead.
    let wrong = http_at(
        "127.0.0.1",
        port,
        "GET",
        "/api/state",
        "toolnet.example",
        &[("Authorization", "Bearer nope")],
        "",
    );
    assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");

    let bearer = format!("Bearer {TEST_TOKEN}");

    // A Host off the allowlist: refused even with the right token.
    let bad_host = http_at(
        "127.0.0.1",
        port,
        "GET",
        "/api/state",
        "evil.example",
        &[("Authorization", &bearer)],
        "",
    );
    assert!(bad_host.starts_with("HTTP/1.1 403"), "{bad_host}");
    // A bare entry matches any port; a port-bearing entry must match exactly.
    let bare_entry = http_at(
        "127.0.0.1",
        port,
        "GET",
        "/api/state",
        "toolnet.example:65000",
        &[("Authorization", &bearer)],
        "",
    );
    assert!(bare_entry.starts_with("HTTP/1.1 200"), "{bare_entry}");
    let port_mismatch = http_at(
        "127.0.0.1",
        port,
        "GET",
        "/api/state",
        "tools.example:1",
        &[("Authorization", &bearer)],
        "",
    );
    assert!(port_mismatch.starts_with("HTTP/1.1 403"), "{port_mismatch}");

    // With the right bearer and Host, every route works.
    let state = http_at(
        "127.0.0.1",
        port,
        "GET",
        "/api/state",
        "toolnet.example",
        &[("Authorization", &bearer)],
        "",
    );
    assert!(state.starts_with("HTTP/1.1 200"), "{state}");
    assert!(state.contains("\"w1\""));
    assert!(state.contains("\"pending\":1"));
    for route in ["/", "/api/doctor", "/api/agents", "/api/tasks/w1"] {
        let got = http_at(
            "127.0.0.1",
            port,
            "GET",
            route,
            "toolnet.example",
            &[("Authorization", &bearer)],
            "",
        );
        assert!(got.starts_with("HTTP/1.1 200"), "{route}: {got}");
    }

    // The POST checks stay on top of the bearer gate.
    let without_furmi_token = http_at(
        "127.0.0.1",
        port,
        "POST",
        "/api/tasks/w1/cancel",
        "toolnet.example",
        &[
            ("Authorization", &bearer),
            ("Origin", "http://toolnet.example"),
        ],
        "",
    );
    assert!(
        without_furmi_token.starts_with("HTTP/1.1 403"),
        "{without_furmi_token}"
    );
    let foreign_origin = http_at(
        "127.0.0.1",
        port,
        "POST",
        "/api/tasks/w1/cancel",
        "toolnet.example",
        &[
            ("Authorization", &bearer),
            ("X-Furmi-Token", TEST_TOKEN),
            ("Origin", "http://evil.example"),
        ],
        "",
    );
    assert!(
        foreign_origin.starts_with("HTTP/1.1 403"),
        "{foreign_origin}"
    );
    let ok = http_at(
        "127.0.0.1",
        port,
        "POST",
        "/api/tasks/w1/cancel",
        "toolnet.example",
        &[
            ("Authorization", &bearer),
            ("X-Furmi-Token", TEST_TOKEN),
            ("Origin", "http://toolnet.example"),
        ],
        "",
    );
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn web_survives_a_corrupt_task_file() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());
    add(home.path(), repo.path(), "w1", "do it");
    std::fs::write(home.path().join("tasks/broken.md"), "not a task").unwrap();

    let port = 18340u16;
    let mut child = furmi(home.path())
        .args(["web", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    http_get(port, "/");

    let state = http_get(port, "/api/state");
    assert!(state.starts_with("HTTP/1.1 200"), "{state}");
    assert!(state.contains("\"w1\""));
    assert!(state.contains("broken.md"), "reported as invalid: {state}");
    assert!(state.contains("\"invalid\":1"));

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn web_serves_the_dashboard_and_state() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    init(home.path());
    add(home.path(), repo.path(), "w1", "do it");

    let port = 18337u16;
    let mut child = furmi(home.path())
        .args(["web", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let index = http_get(port, "/");
    assert!(index.starts_with("HTTP/1.1 200"));
    assert!(index.contains("furmi web"));

    let state = http_get(port, "/api/state");
    assert!(state.starts_with("HTTP/1.1 200"));
    assert!(state.contains("\"w1\""));
    assert!(state.contains("\"pending\":1"));

    let missing = http_get(port, "/nope");
    assert!(missing.starts_with("HTTP/1.1 404"));

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn web_new_task_form_queues_a_task() {
    let home = tempfile::tempdir().unwrap();
    let repo = git_repo();
    init(home.path());

    let port = 18341u16;
    let mut child = furmi(home.path())
        .args(["web", "--no-open", "--port", &port.to_string()])
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let token = page_token(port);

    // Started inside a repo, so the form suggests it.
    let repo_name = repo.path().file_name().unwrap().to_str().unwrap();
    let state = http_get(port, "/api/state");
    assert!(state.contains("\"form\""), "{state}");
    assert!(state.contains(repo_name), "{state}");

    let form = serde_json::json!({
        "prompt": "Tidy up the README\n\nMore detail here.",
        "repo": repo.path(),
        "start": "1h",
        "on_finish": "commit",
        "agent": "claude",
    })
    .to_string();
    let made = http_post(port, &token, "/api/tasks", &form);
    assert!(made.starts_with("HTTP/1.1 200"), "{made}");
    assert!(made.contains("\"id\":\"tidy-up-the-readme\""), "{made}");

    let task = std::fs::read_to_string(home.path().join("tasks/tidy-up-the-readme.md")).unwrap();
    assert!(task.contains("on_finish: commit"), "{task}");
    assert!(task.contains("created_by: web"), "{task}");
    assert!(task.contains("scheduled_at"), "{task}");
    assert!(task.contains("More detail here."), "{task}");
    assert!(http_get(port, "/api/state").contains("\"pending\":1"));

    // Mistakes come back as a message the form can show, not a stack trace.
    for (bad, expect) in [
        (
            r#"{"prompt":"  ","repo":"/tmp","agent":"claude"}"#,
            "prompt is empty",
        ),
        (
            r#"{"prompt":"x","repo":"","agent":"claude"}"#,
            "choose the repository",
        ),
        (
            r#"{"prompt":"x","repo":"/no/such/dir","agent":"claude"}"#,
            "does not exist",
        ),
        (
            r#"{"prompt":"x","repo":"/tmp","start":"soonish"}"#,
            "can't start after",
        ),
        (r#"not json"#, "couldn't read the form"),
    ] {
        let out = http_post(port, &token, "/api/tasks", bad);
        assert!(out.starts_with("HTTP/1.1 400"), "{bad}: {out}");
        assert!(out.contains(expect), "{bad}: {out}");
        assert!(!out.contains("invalid task"), "{bad}: {out}");
    }

    // Like every POST, it needs the page's token.
    let out = http(
        port,
        "POST",
        "/api/tasks",
        &format!("127.0.0.1:{port}"),
        &[("Content-Type", "application/json")],
        &form,
    );
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn start_runs_the_runner_and_the_dashboard_together() {
    let home = tempfile::tempdir().unwrap();
    init(home.path());

    let port = 18342u16;
    let mut child = furmi(home.path())
        .args(["start", "--no-open", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    page_token(port); // waits for the dashboard

    // The runner comes up a moment after the page does.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        let status = stdout(&ok(run(furmi(home.path()).arg("status"))));
        if status.contains("running (pid") || std::time::Instant::now() > deadline {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(status.contains("running (pid"), "{status}");
    assert!(http_get(port, "/api/state").contains("\"alive\":true"));

    // Ctrl-C reaches both in a terminal; here, stop each by hand.
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.path().join("runner.json")).unwrap())
            .unwrap();
    let runner_pid = state["pid"].as_u64().unwrap().to_string();
    child.kill().unwrap();
    child.wait().unwrap();
    Command::new("kill").arg(runner_pid).status().unwrap();
}

/// Waits until `furmi status` reports the runner as `up` (or not), up to five
/// seconds.
fn wait_for_runner(home: &Path, up: bool) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let status = stdout(&ok(run(furmi(home).arg("status"))));
        if status.contains("running (pid") == up || std::time::Instant::now() > deadline {
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn stop_stops_a_runner_and_is_calm_when_there_is_none() {
    let home = tempfile::tempdir().unwrap();
    init(home.path());

    let out = stdout(&ok(run(furmi(home.path()).arg("stop"))));
    assert!(out.contains("isn't running"), "{out}");

    // A runner as `furmi run` would leave it, in the background.
    let mut runner = furmi(home.path())
        .arg("run")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_for_runner(home.path(), true).contains("running (pid"));

    let out = stdout(&ok(run(furmi(home.path()).arg("stop"))));
    assert!(out.contains("Stopped the runner"), "{out}");
    runner.wait().unwrap(); // it really exited
    assert!(wait_for_runner(home.path(), false).contains("NOT running"));

    let out = stdout(&ok(run(furmi(home.path()).arg("stop"))));
    assert!(out.contains("isn't running"), "{out}");
}

#[test]
fn web_can_start_and_stop_the_runner_and_run_the_setup_check() {
    let home = tempfile::tempdir().unwrap();
    init(home.path());

    let port = 18343u16;
    let mut child = furmi(home.path())
        .args(["web", "--no-open", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let token = page_token(port);
    assert!(http_get(port, "/api/state").contains("\"alive\":false"));

    let started = http_post(port, &token, "/api/runner/start", "");
    assert!(started.starts_with("HTTP/1.1 200"), "{started}");
    assert!(started.contains("\"started\":true"), "{started}");
    assert!(http_get(port, "/api/state").contains("\"alive\":true"));
    // Pressing it again is harmless.
    let again = http_post(port, &token, "/api/runner/start", "");
    assert!(again.contains("\"started\":false"), "{again}");

    let stopped = http_post(port, &token, "/api/runner/stop", "");
    assert!(stopped.starts_with("HTTP/1.1 200"), "{stopped}");
    assert!(http_get(port, "/api/state").contains("\"alive\":false"));

    // Same token rule as every other POST.
    let no_token = http(
        port,
        "POST",
        "/api/runner/start",
        &format!("127.0.0.1:{port}"),
        &[],
        "",
    );
    assert!(no_token.starts_with("HTTP/1.1 403"), "{no_token}");

    let doctor = http_get(port, "/api/doctor");
    assert!(doctor.starts_with("HTTP/1.1 200"), "{doctor}");
    assert!(doctor.contains("config.toml is valid"), "{doctor}");
    assert!(doctor.contains("\"level\":\"ok\""), "{doctor}");
    // Whether a runner is up is the dashboard's own banner, not a check.
    assert!(!doctor.contains("runner"), "{doctor}");

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn web_edits_waiting_tasks_and_refuses_started_ones() {
    let home = tempfile::tempdir().unwrap();
    let repo = git_repo();
    init(home.path());
    add(home.path(), repo.path(), "w1", "first draft");

    let port = 18344u16;
    let mut child = furmi(home.path())
        .args(["web", "--no-open", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let token = page_token(port);

    let detail = http_get(port, "/api/tasks/w1");
    assert!(detail.starts_with("HTTP/1.1 200"), "{detail}");
    assert!(detail.contains("first draft"), "{detail}");
    assert!(detail.contains("\"on_finish\":\"none\""), "{detail}");

    let form = serde_json::json!({
        "prompt": "second draft",
        "repo": repo.path(),
        "start": "2h",
        "on_finish": "push",
    })
    .to_string();
    let out = http_post(port, &token, "/api/tasks/w1/edit", &form);
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    let task = std::fs::read_to_string(home.path().join("tasks/w1.md")).unwrap();
    assert!(
        task.contains("second draft") && !task.contains("first draft"),
        "{task}"
    );
    assert!(
        task.contains("on_finish: push") && task.contains("scheduled_at"),
        "{task}"
    );

    // Mistakes are explained, and the file stays as it was.
    let bad = serde_json::json!({ "prompt": "  ", "repo": repo.path() }).to_string();
    let out = http_post(port, &token, "/api/tasks/w1/edit", &bad);
    assert!(
        out.starts_with("HTTP/1.1 400") && out.contains("prompt is empty"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("tasks/w1.md")).unwrap(),
        task
    );

    // Once it's no longer waiting, it can't be edited.
    http_post(port, &token, "/api/tasks/w1/cancel", "");
    let out = http_post(port, &token, "/api/tasks/w1/edit", &form);
    assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    assert!(http_get(port, "/api/tasks/w1").starts_with("HTTP/1.1 404"));

    // Same token rule as everything else that changes things.
    let out = http(
        port,
        "POST",
        "/api/tasks/w1/edit",
        &format!("127.0.0.1:{port}"),
        &[],
        &form,
    );
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn web_manages_schedules_end_to_end() {
    let home = tempfile::tempdir().unwrap();
    let repo = git_repo();
    init(home.path());

    let port = 18345u16;
    let mut child = furmi(home.path())
        .args(["web", "--no-open", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let token = page_token(port);
    let form = |prompt: &str, cron: &str| {
        serde_json::json!({ "prompt": prompt, "repo": repo.path(), "cron": cron, "on_finish": "pr" })
            .to_string()
    };

    let out = http_post(
        port,
        &token,
        "/api/schedules",
        &form("Update deps\n\nThen test", "0 9 * * 1-5"),
    );
    assert!(
        out.starts_with("HTTP/1.1 200") && out.contains("\"id\":\"update-deps\""),
        "{out}"
    );
    let file = std::fs::read_to_string(home.path().join("schedules/update-deps.md")).unwrap();
    assert!(
        file.contains("cron: 0 9 * * 1-5") || file.contains("cron: '0 9 * * 1-5'"),
        "{file}"
    );
    assert!(
        file.contains("on_finish: pr") && file.contains("Then test"),
        "{file}"
    );

    let state = http_get(port, "/api/state");
    assert!(state.contains("\"schedules\":1"), "{state}");
    assert!(state.contains("\"summary\":\"Update deps\""), "{state}");
    assert!(state.contains("\"enabled\":true"), "{state}");

    let detail = http_get(port, "/api/schedules/update-deps");
    assert!(
        detail.contains("\"cron\":\"0 9 * * 1-5\"") && detail.contains("Then test"),
        "{detail}"
    );

    let out = http_post(
        port,
        &token,
        "/api/schedules/update-deps/edit",
        &form("Update deps weekly", "0 8 * * 1"),
    );
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(http_get(port, "/api/schedules/update-deps").contains("0 8 * * 1"));

    let out = http_post(
        port,
        &token,
        "/api/schedules/update-deps/edit",
        &form("x", "every night"),
    );
    assert!(
        out.starts_with("HTTP/1.1 400") && out.contains("invalid cron"),
        "{out}"
    );

    http_post(port, &token, "/api/schedules/update-deps/pause", "");
    assert!(http_get(port, "/api/state").contains("\"enabled\":false"));
    http_post(port, &token, "/api/schedules/update-deps/resume", "");
    assert!(http_get(port, "/api/state").contains("\"enabled\":true"));

    let out = http_post(port, &token, "/api/schedules/update-deps/delete", "");
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(!home.path().join("schedules/update-deps.md").exists());
    let out = http_post(port, &token, "/api/schedules/update-deps/delete", "");
    assert!(
        out.starts_with("HTTP/1.1 400") && out.contains("no schedule"),
        "{out}"
    );

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn web_lists_agents_and_sets_the_default() {
    let home = tempfile::tempdir().unwrap();
    init(home.path());

    let port = 18346u16;
    let mut child = furmi(home.path())
        .args(["web", "--no-open", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let token = page_token(port);

    let agents = http_get(port, "/api/agents");
    assert!(agents.starts_with("HTTP/1.1 200"), "{agents}");
    for name in ["claude", "codex", "gemini", "opencode", "pi"] {
        assert!(agents.contains(&format!("\"name\":\"{name}\"")), "{agents}");
    }
    assert!(agents.contains("npm install -g @openai/codex"), "{agents}");
    assert!(agents.contains("\"is_default\":true"), "{agents}"); // claude, from init

    let out = http_post(port, &token, "/api/agents/default", r#"{"agent":"codex"}"#);
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    let config = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(config.contains("agent = \"codex\""), "{config}");
    assert!(!config.contains("agent = \"claude\""), "{config}");

    let out = http_post(
        port,
        &token,
        "/api/agents/default",
        r#"{"agent":"nonesuch"}"#,
    );
    assert!(
        out.starts_with("HTTP/1.1 400") && out.contains("unknown agent"),
        "{out}"
    );
    let out = http(
        port,
        "POST",
        "/api/agents/default",
        &format!("127.0.0.1:{port}"),
        &[],
        r#"{"agent":"pi"}"#,
    );
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");

    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn furmi_agents_lists_installs_and_picks_the_default() {
    let home = tempfile::tempdir().unwrap();
    // A PATH with nothing on it: no agent is installed.
    let bare = |home: &Path| {
        let mut cmd = furmi(home);
        cmd.env("PATH", "");
        cmd
    };

    let out = stdout(&ok(run(bare(home.path()).arg("agents"))));
    assert!(
        out.contains("AGENT") && out.contains("HOW TO INSTALL"),
        "{out}"
    );
    assert!(
        out.contains("npm install -g @anthropic-ai/claude-code"),
        "{out}"
    );
    assert!(out.contains("not installed"), "{out}");
    assert!(
        out.contains("needs Node.js") || out.contains("it needs Node.js"),
        "{out}"
    );
    // First use set the home up, and found nothing to default to.
    assert!(home.path().join("config.toml").is_file());

    let out = stdout(&ok(run(bare(home.path()).args(["agents", "use", "codex"]))));
    assert!(out.contains("`codex` is now the default"), "{out}");
    assert!(
        out.contains("isn't installed yet") && out.contains("@openai/codex"),
        "{out}"
    );
    let out = stdout(&ok(run(bare(home.path()).arg("agents"))));
    assert!(out.contains("not installed (default)"), "{out}");

    let out = failed(run(bare(home.path()).args(["agents", "use", "nonesuch"])));
    assert!(stderr(&out).contains("unknown agent"), "{}", stderr(&out));
}
