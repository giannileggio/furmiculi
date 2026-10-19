//! Thin wrapper over the `tmux` CLI. Every call passes an argv, never a
//! shell string. Targets use tmux's `=` prefix for exact name matches, so a
//! window named `123` is never mistaken for window index 123.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::fsutil::run_with_timeout;

/// A tmux call that takes longer than this has hung (a wedged server).
const TMUX_TIMEOUT: Duration = Duration::from_secs(20);

/// Only liveness is reported: tmux can mark a pane dead before it has
/// reaped the process, so its exit status isn't reliable. `furmi exec` records
/// the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub window: String,
    pub dead: bool,
}

fn tmux(args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("tmux");
    cmd.args(args);
    run_with_timeout(cmd, TMUX_TIMEOUT).map_err(|e| match e.downcast_ref::<std::io::Error>() {
        Some(_) => e.context("running tmux (is it installed?)"),
        None => e.context(format!("tmux {}", args.first().unwrap_or(&""))),
    })
}

fn check(out: Output, what: &str) -> Result<Output> {
    if !out.status.success() {
        bail!(
            "tmux {what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

/// Whether `session` exists. `Ok(false)` means tmux says the session (or
/// the whole server) is genuinely gone; `Err` means tmux itself failed
/// (timed out, isn't installed, a transient error). Callers must not
/// confuse the two, or a hiccup in tmux tears down healthy running tasks.
pub fn session_exists(session: &str) -> Result<bool> {
    let out = tmux(&["has-session", "-t", &format!("={session}")])?;
    if out.status.success() {
        return Ok(true);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if is_missing(&err) {
        return Ok(false);
    }
    bail!("tmux has-session failed: {}", err.trim())
}

/// Whether tmux's message means the session or the whole server is gone,
/// as opposed to a failure worth retrying on the next pass. Only tmux's own
/// "no such session/server" wording qualifies; anything else (a wrapper
/// exiting non-zero, a killed client, empty stderr) is an error.
fn is_missing(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "can't find session",
        "can't find window",
        "no server running",
        "no current session",
        "session not found",
        "error connecting to",
        "lost server",
    ];
    let text = text.to_ascii_lowercase();
    MARKERS.iter().any(|m| text.contains(m))
}

/// Starts `argv` in a new detached window named `window`. The window stays
/// around after the process exits (`remain-on-exit`) so its output can be
/// captured; setting that option in the same tmux invocation means even an
/// instantly-exiting process can't beat it.
pub fn spawn(session: &str, window: &str, workdir: &Path, argv: &[String]) -> Result<()> {
    let workdir = workdir.to_str().context("workdir is not valid UTF-8")?;
    let session_target = format!("={session}:");
    let window_target = format!("={session}:={window}");
    let mut args = if session_exists(session)? {
        vec!["new-window", "-d", "-t", &session_target]
    } else {
        vec!["new-session", "-d", "-s", session]
    };
    args.extend(["-n", window, "-c", workdir]);
    let argv: Vec<String> = argv.iter().map(|a| escape_arg(a)).collect();
    args.extend(argv.iter().map(String::as_str));
    args.extend([
        ";",
        "set-option",
        "-w",
        "-t",
        &window_target,
        "remain-on-exit",
        "on",
    ]);
    check(tmux(&args)?, "spawn")?;
    Ok(())
}

/// tmux treats an argument ending in `;` as a command separator and turns a
/// trailing `\;` into a literal `;`.
fn escape_arg(arg: &str) -> String {
    match arg.strip_suffix(';') {
        Some(rest) => format!("{rest}\\;"),
        None => arg.to_string(),
    }
}

/// Every pane in `session`. A missing session yields no panes.
pub fn panes(session: &str) -> Result<Vec<Pane>> {
    if !session_exists(session)? {
        return Ok(Vec::new());
    }
    let out = tmux(&[
        "list-panes",
        "-s",
        "-t",
        &format!("={session}"),
        "-F",
        "#{pane_dead} #{window_name}",
    ])?;
    if !out.status.success() {
        // The session can vanish between the two calls. Any other failure
        // is transient and must reach the caller as an error.
        let err = String::from_utf8_lossy(&out.stderr);
        if is_missing(&err) {
            return Ok(Vec::new());
        }
        bail!("tmux list-panes failed: {}", err.trim());
    }
    Ok(parse_panes(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_panes(text: &str) -> Vec<Pane> {
    text.lines()
        .filter_map(|line| {
            let (dead, window) = line.split_once(' ')?;
            Some(Pane {
                window: window.to_string(),
                dead: dead == "1",
            })
        })
        .collect()
}

/// The pane's full scrollback, for the task log.
pub fn capture(session: &str, window: &str) -> Result<String> {
    let out = check(
        tmux(&[
            "capture-pane",
            "-p",
            "-J",
            "-S",
            "-",
            "-t",
            &format!("={session}:={window}"),
        ])?,
        "capture-pane",
    )?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Replaces this process with a tmux client showing `session`, focused on
/// `window` if given. Inside tmux, switches the current client instead of
/// nesting. Returns only on failure.
pub fn attach(session: &str, window: Option<&str>) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let session_target = format!("={session}");
    let mut args = Vec::new();
    let window_target;
    if let Some(w) = window {
        window_target = format!("={session}:={w}");
        args.extend(["select-window", "-t", &window_target, ";"]);
    }
    let client = if std::env::var_os("TMUX").is_some() {
        "switch-client"
    } else {
        "attach-session"
    };
    args.extend([client, "-t", &session_target]);
    let err = Command::new("tmux").args(&args).exec();
    anyhow::Error::new(err).context("running tmux (is it installed?)")
}

/// Stops what runs in the window, then removes it. See [`stop_processes`].
pub fn terminate_window(session: &str, window: &str, grace: Duration) -> Result<()> {
    stop_processes(session, window, grace);
    kill_window(session, window)
}

/// SIGTERM to the pane's whole process group, then SIGKILL if it's still
/// there after `grace`. (A bare `kill-window` only sends SIGHUP, which an
/// agent can ignore.) The window itself stays, so its output can still be
/// captured. A dead or missing window is a no-op.
pub fn stop_processes(session: &str, window: &str, grace: Duration) {
    if let Some(pid) = pane_pid(session, window) {
        let group_alive = || {
            // SAFETY: signal 0 only checks that the group exists.
            unsafe { libc::kill(-pid, 0) == 0 }
        };
        // SAFETY: plain libc calls; the pane's process leads its own group.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        let deadline = Instant::now() + grace;
        while group_alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if group_alive() {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    }
}

/// The pid of the process the window was started with, if it's still alive.
fn pane_pid(session: &str, window: &str) -> Option<i32> {
    let out = tmux(&[
        "list-panes",
        "-t",
        &format!("={session}:={window}"),
        "-F",
        "#{pane_dead} #{pane_pid}",
    ])
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (dead, pid) = text.lines().next()?.split_once(' ')?;
    (dead == "0").then(|| pid.trim().parse().ok()).flatten()
}

/// Kills the window. A window that's already gone is not an error.
///
/// With no session there's nothing to kill. (tmux words that differently
/// depending on whether its socket directory exists, e.g. on a fresh boot,
/// so asking first is more robust than matching error text.)
pub fn kill_window(session: &str, window: &str) -> Result<()> {
    if !session_exists(session)? {
        return Ok(());
    }
    let out = tmux(&["kill-window", "-t", &format!("={session}:={window}")])?;
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.success() || is_missing(&err) {
        Ok(())
    } else {
        bail!("tmux kill-window failed: {}", err.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn tells_a_missing_session_from_a_transient_failure() {
        assert!(is_missing("can't find session: furmit"));
        assert!(is_missing("can't find window: furmit"));
        assert!(is_missing("no server running on /tmp/tmux-1000/default"));
        assert!(is_missing(
            "error connecting to /tmp/tmux-1000/default (No such file or directory)"
        ));
        assert!(!is_missing("tmux: transient failure"));
        assert!(!is_missing(""));
    }

    #[test]
    fn parses_list_panes_output() {
        let panes = parse_panes("0 running-task\n1 has space\n");
        assert_eq!(
            panes,
            vec![
                Pane {
                    window: "running-task".into(),
                    dead: false
                },
                Pane {
                    window: "has space".into(),
                    dead: true
                },
            ]
        );
    }

    struct Session(String);
    impl Drop for Session {
        fn drop(&mut self) {
            let _ = tmux(&["kill-session", "-t", &format!("={}", self.0)]);
        }
    }

    fn wait_dead(session: &str, window: &str) -> Pane {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(p) = panes(session)
                .unwrap()
                .into_iter()
                .find(|p| p.window == window)
                && p.dead
            {
                return p;
            }
            assert!(Instant::now() < deadline, "{window} never exited");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn finished_windows_stay_until_killed() {
        let s = Session(format!("furmi-test-{}", std::process::id()));
        let dir = std::env::temp_dir();
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // First spawn creates the session, second adds a window to it.
        spawn(&s.0, "ok", &dir, &argv(&["true"])).unwrap();
        spawn(&s.0, "123", &dir, &argv(&["echo", "semi;", "back\\;"])).unwrap();

        wait_dead(&s.0, "ok");
        wait_dead(&s.0, "123"); // found by name, not as window index 123
        assert!(capture(&s.0, "123").unwrap().contains("semi; back\\;"));

        kill_window(&s.0, "ok").unwrap();
        kill_window(&s.0, "ok").unwrap(); // already gone: still fine
        assert!(panes(&s.0).unwrap().iter().all(|p| p.window != "ok"));
    }

    #[test]
    fn terminate_window_escalates_past_ignored_signals() {
        let s = Session(format!("furmi-test-term-{}", std::process::id()));
        let dir = std::env::temp_dir();
        let argv = ["sh", "-c", "trap '' HUP TERM; sleep 60 & wait"]
            .map(String::from)
            .to_vec();
        spawn(&s.0, "stubborn", &dir, &argv).unwrap();
        std::thread::sleep(Duration::from_millis(300)); // let the trap install

        let started = Instant::now();
        terminate_window(&s.0, "stubborn", Duration::from_millis(500)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            !panes(&s.0)
                .unwrap_or_default()
                .iter()
                .any(|p| p.window == "stubborn")
        );
    }
}
