//! Reaping running tasks: how an attempt ended, and why.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, FixedOffset};

use crate::config::Config;
use crate::fsutil;
use crate::home::Home;
use crate::spec::{Effective, Mode};
use crate::task::{Status, Task};
use crate::tmux;

use super::exec::{Exit, read_exit};
use super::finishing::finish;
use super::log::log;
use super::usage::read_usage;
use super::{load_tasks, log_file, status_file, usage_file};

/// How long a stopped agent gets to exit after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);

pub(super) enum Outcome {
    Succeeded(i32),
    Failed {
        error: String,
        exit_code: Option<i32>,
        /// Whether a retry could plausibly help. A provider rejecting the
        /// model won't change on a retry, so those failures skip it.
        retry: bool,
    },
    /// An interactive session ended without `furmi done` / `furmi fail`.
    NeedsReview,
    /// The attempt was cut short from outside (its tmux window vanished or
    /// was killed, e.g. by a reboot): requeued without using up a retry.
    Interrupted(String),
}

/// Reaps finished tasks. Returns the successful ones, whose `on_finish` is
/// still to run (see `tick`).
pub(super) fn poll_running(
    home: &Home,
    config: &Config,
    now: DateTime<FixedOffset>,
) -> Result<Vec<(PathBuf, Task)>> {
    let mut panes: HashMap<String, Option<Vec<tmux::Pane>>> = HashMap::new();
    let mut succeeded = Vec::new();
    for (path, mut task) in load_tasks(home, Status::Running)? {
        let (session, window) = window_of(&task, config);
        // A tmux failure (timeout, a transient error) is not evidence that
        // the window is gone: leave the session's tasks untouched and retry
        // on the next pass, instead of requeueing healthy runs.
        let session_panes = panes.entry(session.clone()).or_insert_with(|| {
            tmux::panes(&session)
                .map_err(|e| {
                    log(&format!(
                        "{session}: could not list its tmux windows ({e:#}); leaving its \
                         running tasks alone until the next pass"
                    ));
                })
                .ok()
        });
        let Some(session_panes) = session_panes else {
            continue;
        };
        let pane = session_panes.iter().find(|p| p.window == window);
        let eff = task.spec.resolve(&config.defaults);
        // A record that can't be read or parsed (hand-edited or
        // corrupted) counts as no record yet: the attempt may still
        // be running, and a dead pane without one is handled below.
        // Failing the whole pass here would stall every task behind
        // one bad file, forever.
        let exit = match read_exit(&status_file(home, &task.id, task.attempts)) {
            Ok(exit) => exit,
            Err(e) => {
                log(&format!(
                    "{}: ignoring an unreadable exit record: {e:#}",
                    task.id
                ));
                None
            }
        };
        if exit.is_some()
            && let Some(usage) = read_usage(&usage_file(home, &task.id, task.attempts))
        {
            task.add_usage(usage.tokens_in, usage.tokens_out, usage.cost_usd);
            task.set_model_used(usage.model.as_deref());
        }
        let mut stop_agent = false;
        let mut outcome = match (exit, pane) {
            (Some(exit), _) => exit_outcome(eff.mode, exit),
            (None, None) => Outcome::Interrupted("tmux window disappeared".into()),
            // `furmi exec` writes the status before exiting, so a dead pane
            // without one means `furmi exec` itself was killed (or the agent
            // killed its own process group) before it could record it.
            (None, Some(p)) if p.dead => Outcome::Interrupted(
                "the window died without recording an exit status (`furmi exec` was killed or \
                 the agent crashed before it could write one)"
                    .into(),
            ),
            (None, Some(_)) if eff.mode == Mode::Headless && timed_out(&task, &eff, now) => {
                stop_agent = true;
                Outcome::Failed {
                    error: format!(
                        "timed out after {}",
                        humantime::format_duration(eff.timeout)
                    ),
                    exit_code: None,
                    retry: true,
                }
            }
            (None, Some(_)) => continue,
        };
        if pane.is_some() {
            if stop_agent {
                tmux::stop_processes(&session, &window, STOP_GRACE);
            }
            save_log(home, &task, &session, &window);
            // If tmux fails here the task's file hasn't been touched, so
            // leaving it running lets the next pass close the window and
            // record the outcome.
            if let Err(e) = tmux::kill_window(&session, &window) {
                log(&format!(
                    "{}: could not close its tmux window ({e:#}); retrying on the next pass",
                    task.id
                ));
                continue;
            }
        }
        // When an attempt failed, prefer a provider error found in its log
        // (a rejected model, missing credentials) over the bare exit code.
        // Permanent ones aren't retried: the identical model would fail the
        // same way. Temporary ones (a rate limit, an overloaded provider)
        // are. Only failed attempts are scanned: a successful run's output
        // may mention these phrases (e.g. while working on code that does).
        if matches!(outcome, Outcome::Failed { .. })
            && let Some((error, retry)) = provider_error(home, &task)
        {
            outcome = Outcome::Failed {
                error,
                exit_code: exit_code_of(exit),
                retry,
            };
        }
        match outcome {
            Outcome::Succeeded(code) => {
                task.exit_code = Some(code);
                succeeded.push((path, task));
            }
            outcome => {
                if let Err(e) = finish(home, config, &path, task, outcome, now) {
                    log(&format!("finishing {}: {e:#}", path.display()));
                }
            }
        }
    }
    Ok(succeeded)
}

fn exit_code_of(exit: Option<Exit>) -> Option<i32> {
    match exit {
        Some(Exit::Code(code)) => Some(code),
        _ => None,
    }
}

/// A provider- or model-level error the agent reported, as an actionable
/// message, and whether a retry could help. `None` if the attempt's log has
/// no such line (or there is no log).
fn provider_error(home: &Home, task: &Task) -> Option<(String, bool)> {
    let path = log_file(home, &task.id, task.attempts);
    let line = provider_error_line(&read_log_tail(&path)?)?;
    Some(if is_transient(&line) {
        (
            format!("the agent's provider reported a temporary error: {line}"),
            true,
        )
    } else {
        (
            format!(
                "the agent's provider rejected the run: {line}. Retrying the same model would \
                 fail the same way, so it wasn't retried: fix the provider or set a different \
                 `model`, then run `furmi retry {id}`",
                id = task.id
            ),
            false,
        )
    })
}

/// Wording of a provider error that clears up by itself, so a retry is worth
/// it even though the line also looks like a rejection.
const TRANSIENT_MARKERS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "too many requests",
    "overloaded",
    "try again",
    "temporarily",
    "timed out",
    "timeout",
    "service unavailable",
    "capacity",
];

fn is_transient(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    TRANSIENT_MARKERS.iter().any(|m| lower.contains(m))
}

/// Signatures of an error only the provider/model can explain, e.g.
/// `Error from provider (Console): This model is not available in your
/// country`. Matched case-insensitively against each line of the agent's
/// output.
const PROVIDER_ERROR_MARKERS: &[&str] = &[
    "error from provider",
    "model is not available",
    "not available in your country",
    "invalid_api_key",
    "invalid api key",
    "incorrect api key",
    "authentication_error",
    "insufficient_quota",
    "insufficient quota",
    "model not found",
    "unknown model",
    "no such model",
];

/// The first provider-error line in `text`, with terminal escapes removed.
fn provider_error_line(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = strip_ansi(line);
        let lower = line.to_ascii_lowercase();
        PROVIDER_ERROR_MARKERS
            .iter()
            .any(|m| lower.contains(m))
            .then(|| line.trim().to_string())
    })
}

/// Removes ANSI CSI/OSC escape sequences, so error messages taken from an
/// agent's (coloured) output stay readable in the task file and dashboard.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                for c in chars.by_ref() {
                    if c == '\u{7}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The last part of an attempt's log: enough to find a provider error
/// without reading a whole (possibly huge) file into memory.
fn read_log_tail(path: &Path) -> Option<String> {
    const TAIL_BYTES: u64 = 256 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > TAIL_BYTES {
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(len - TAIL_BYTES)).ok()?;
    }
    let mut buf = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Appends a runner note to the attempt's log, so the pane capture alone
/// ("Pane is dead (status 1 ...)") isn't the only explanation of how the
/// attempt ended.
pub(super) fn note_in_log(home: &Home, task: &Task, note: &str) {
    let path = log_file(home, &task.id, task.attempts);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = writeln!(file, "\n[furmi] {note}");
}

fn exit_outcome(mode: Mode, exit: Exit) -> Outcome {
    match (mode, exit) {
        (Mode::Interactive, _) => Outcome::NeedsReview,
        (Mode::Headless, Exit::Code(0)) => Outcome::Succeeded(0),
        (Mode::Headless, Exit::Code(n)) => Outcome::Failed {
            error: format!("exited with status {n}"),
            exit_code: Some(n),
            retry: true,
        },
        (Mode::Headless, Exit::Signal(s)) => Outcome::Failed {
            error: format!("killed by signal {s}"),
            exit_code: None,
            retry: true,
        },
    }
}

fn timed_out(task: &Task, eff: &Effective, now: DateTime<FixedOffset>) -> bool {
    task.started_at
        .and_then(|s| (now - s).to_std().ok())
        .is_some_and(|elapsed| elapsed > eff.timeout)
}

/// `(session, window)` the task runs in: what was recorded at start, or the
/// configured session and the task id.
pub fn window_of(task: &Task, config: &Config) -> (String, String) {
    match task.tmux_window.as_deref().and_then(|w| w.split_once(':')) {
        Some((s, w)) => (s.to_string(), w.to_string()),
        None => (config.runner.tmux_session.clone(), task.id.clone()),
    }
}

/// Saves the attempt's output: the pane's scrollback, unless `furmi exec`
/// already streamed the full output to the log file (headless runs).
pub fn save_log(home: &Home, task: &Task, session: &str, window: &str) {
    let path = log_file(home, &task.id, task.attempts);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
        return;
    }
    let result = tmux::capture(session, window).and_then(|text| fsutil::write_atomic(&path, text));
    if let Err(e) = result {
        log(&format!("{}: could not save the log: {e:#}", task.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_provider_error_and_strips_its_colours() {
        let log = "normal output\n\u{1b}[91m\u{1b}[1mError: \u{1b}[0mError from provider (Console): \
                   This model is not available in your country\nmore output\n";
        let line = provider_error_line(log).unwrap();
        assert_eq!(
            line,
            "Error: Error from provider (Console): This model is not available in your country"
        );
        assert!(!line.contains('\u{1b}'));
    }
    #[test]
    fn a_rate_limit_is_transient_and_a_rejected_model_is_not() {
        assert!(is_transient(
            "Error: Error from provider (Console): Rate limit exceeded. Please try again later."
        ));
        assert!(is_transient("overloaded_error: Overloaded"));
        assert!(!is_transient(
            "Error from provider (Console): This model is not available in your country"
        ));
        assert!(!is_transient("invalid api key"));
    }
    #[test]
    fn no_provider_error_in_ordinary_output() {
        assert!(provider_error_line("all good\nwrote 3 files\n").is_none());
        assert!(provider_error_line("model = \"gpt\"\n").is_none());
    }
    #[test]
    fn strips_common_escape_sequences() {
        assert_eq!(strip_ansi("a\u{1b}[31mred\u{1b}[0mb"), "aredb");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}after"), "after");
    }
    #[test]
    fn maps_exit_status_to_outcome() {
        use Exit::*;
        assert!(matches!(
            exit_outcome(Mode::Headless, Code(0)),
            Outcome::Succeeded(0)
        ));
        assert!(matches!(
            exit_outcome(Mode::Headless, Code(2)),
            Outcome::Failed {
                exit_code: Some(2),
                ..
            }
        ));
        assert!(matches!(
            exit_outcome(Mode::Headless, Signal(9)),
            Outcome::Failed { .. }
        ));
        assert!(matches!(
            exit_outcome(Mode::Interactive, Code(0)),
            Outcome::NeedsReview
        ));
    }
}
