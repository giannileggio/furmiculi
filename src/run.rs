//! `furmi run`: enqueue due schedules, poll running tasks, start pending ones.
//!
//! Each task runs in its own tmux window, named after the task id. The
//! window runs `furmi exec <id>`, which loads the task, runs the agent and
//! records its exit status in `logs/<id>.<attempt>.exit`. So the prompt
//! never passes through tmux's command line (which splits arguments ending
//! in `;` and caps message size), and the outcome doesn't depend on tmux's
//! own exit-status tracking (which lags behind `pane_dead`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, Local};
use serde::{Deserialize, Serialize};

use crate::cmd::{md_files, unique_id};
use crate::config::{Agent, Config};
use crate::fsutil;
use crate::git;
use crate::home::Home;
use crate::schedule::Schedule;
use crate::spec::{Effective, Mode};
use crate::task::{Status, Task, now};
use crate::tmux;

/// How long a stopped agent gets to exit after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// Interruptions (see [`Outcome::Interrupted`]) a task absorbs before the
/// next one counts as a failure, so a task that keeps losing its window
/// can't loop forever.
const MAX_INTERRUPTIONS: u32 = 5;

/// Set from the SIGTERM/SIGINT handler (see [`install_shutdown_handler`]).
/// The runner checks it between passes, so a signal can't tear down a
/// half-finished tick or the tmux windows running tasks live in.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Turns SIGTERM/SIGINT into a request to stop after the current pass.
/// Without it the process dies mid-tick, which can leave a task file
/// written but not yet archived. Running tasks keep going in their tmux
/// windows either way; the next `furmi run` picks them up.
pub fn install_shutdown_handler() {
    // SAFETY: `on_shutdown` only stores to an atomic, which is
    // async-signal-safe; the previous disposition was the default.
    unsafe {
        let handler = on_shutdown as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

/// The first signal asks for a clean stop. A second one means the pass is
/// stuck (e.g. a slow push) and the user wants out now: exit immediately
/// (`_exit` is async-signal-safe). Running tasks live in tmux and survive.
extern "C" fn on_shutdown(_signal: libc::c_int) {
    if SHUTDOWN.swap(true, Ordering::Relaxed) {
        // SAFETY: `_exit` is async-signal-safe and never returns.
        unsafe { libc::_exit(130) }
    }
}

/// Whether a stop has been requested (SIGTERM/SIGINT).
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

pub fn run(home: &Home, once: bool) -> Result<()> {
    home.ensure_initialized()?;
    let _runner = home.claim_runner()?;
    install_shutdown_handler();
    let _ = LOG_FILE.set(home.runner_log());
    let mut config = Config::load(&home.config_path())?;
    let mut state = RunnerState {
        pid: std::process::id(),
        started_at: now(),
        last_tick_at: None,
        last_error: None,
    };
    if !once {
        log(&format!(
            "watching {} every {}",
            home.root().display(),
            humantime::format_duration(config.runner.poll_interval)
        ));
    }
    loop {
        let result = tick(home, &config);
        state.last_tick_at = Some(now());
        state.last_error = result.as_ref().err().map(|e| format!("{e:#}"));
        if !once {
            state.save(home);
        }
        if let Err(e) = result {
            if once {
                return Err(e);
            }
            log(&format!("error: {e:#}"));
        }
        if once {
            return Ok(());
        }
        if !sleep_until_tick(config.runner.poll_interval) {
            log(
                "stopping on request: running tasks keep going in their tmux windows; \
                 `furmi run` will pick them up again",
            );
            return Ok(());
        }
        match Config::load(&home.config_path()) {
            Ok(c) => config = c,
            Err(e) => log(&format!("keeping the previous config: {e:#}")),
        }
    }
}

/// Sleeps up to `duration`, returning `false` as soon as a stop is
/// requested. Short slices keep a long poll interval from delaying a
/// signal-driven stop.
fn sleep_until_tick(duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if shutdown_requested() {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
    !shutdown_requested()
}

/// The runner's heartbeat, in `runner.json`: lets `furmi status` tell a live
/// runner from a dead or wedged one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerState {
    pub pid: u32,
    pub started_at: DateTime<FixedOffset>,
    pub last_tick_at: Option<DateTime<FixedOffset>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl RunnerState {
    fn save(&self, home: &Home) {
        let written = serde_json::to_string(self)
            .map_err(anyhow::Error::from)
            .and_then(|json| fsutil::write_atomic(&home.runner_state(), json));
        if let Err(e) = written {
            log(&format!("could not write the heartbeat: {e:#}"));
        }
    }

    pub fn load(home: &Home) -> Option<Self> {
        let text = std::fs::read_to_string(home.runner_state()).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// Runs inside the task's tmux window: runs the agent, then writes how it
/// ended to `status_file` for [`poll_running`] to pick up. A headless
/// agent's stdout and stderr are also streamed to the attempt's log file as
/// they arrive (see [`StreamedLog`]), so the log survives tmux dying and
/// isn't limited by its scrollback. What that stdout said is parsed too (see
/// [`Stream`]): token/cost usage goes next to the status, at
/// [`usage_file_for`], and a failed run reported only in an event stream
/// still ends up as a failure.
pub fn exec(home: &Home, id: &str, status_file: &Path) -> Result<ExitCode> {
    let spawned = (|| {
        let config = Config::load(&home.config_path())?;
        let task = Task::load(&home.tasks().join(format!("{id}.md")))?;
        let eff = task.spec.resolve(&config.defaults);
        let name = eff.agent.as_deref().context("no agent set")?;
        let agent = config
            .agents
            .get(name)
            .with_context(|| format!("unknown agent `{name}`"))?;
        let model = eff.model.as_deref().or(agent.model.as_deref());
        let argv = agent_argv(agent, eff.mode, &task.prompt, model, &task.id);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env("FURMI_HOME", home.root())
            .env("FURMI_TASK_ID", &task.id);
        // Headless output is captured (and teed back below) so it can be
        // logged and a final JSON summary parsed for usage. Interactive
        // mode keeps a real, fully inherited tty: it's a live session for a
        // human to use.
        if eff.mode == Mode::Headless {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("starting `{}`", argv[0]))?;
        Ok::<_, anyhow::Error>((child, eff.mode))
    })();
    let (exit, usage) = match spawned {
        Ok((mut child, mode)) => {
            // Ctrl-C / Ctrl-\ in the window are meant for the agent; this
            // process must outlive it to record the status. Ignoring them
            // only after the spawn keeps the child's handlers default.
            // SAFETY: plain libc calls with valid constants.
            unsafe {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                libc::signal(libc::SIGQUIT, libc::SIG_IGN);
            }
            let tees = (mode == Mode::Headless).then(|| {
                let log = Arc::new(Mutex::new(StreamedLog::create(
                    &status_file.with_extension("log"),
                )));
                let stdout = child.stdout.take().expect("piped for headless mode");
                let stderr = child.stderr.take().expect("piped for headless mode");
                (
                    tee(stdout, std::io::stdout(), log.clone(), true),
                    tee(stderr, std::io::stderr(), log, false),
                )
            });
            let status = child.wait().context("waiting for the agent")?;
            let captured = tees.and_then(|(out, err)| {
                let _ = err.join();
                out.join().ok()
            });
            let exit = match (status.code(), status.signal()) {
                (Some(code), _) => Exit::Code(code),
                (None, Some(sig)) => Exit::Signal(sig),
                (None, None) => Exit::Code(1),
            };
            let stream = captured.map(|out| Stream::parse(&out)).unwrap_or_default();
            let exit = match (exit, &stream.error) {
                // pi's `--mode json` exits 0 even when the run failed; the
                // stream is the only place that says so. Anything else (a
                // non-zero exit) already speaks for itself.
                (Exit::Code(0), Some(error)) => {
                    eprintln!("error: {error}");
                    Exit::Code(1)
                }
                (exit, _) => exit,
            };
            (exit, stream.usage)
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            (Exit::Code(127), None)
        }
    };
    // Written before the exit record, so a poller that sees the exit record
    // finds any usage already there.
    if let Some(usage) = usage {
        let path = usage_file_for(status_file);
        if let Err(e) = fsutil::write_atomic(&path, usage.to_json()) {
            eprintln!("warning: could not write {}: {e:#}", path.display());
        }
    }
    fsutil::write_atomic(status_file, exit.to_string())?;
    Ok(match exit {
        Exit::Code(c) => ExitCode::from(c.clamp(0, 255) as u8),
        Exit::Signal(_) => ExitCode::FAILURE,
    })
}

/// Most of an attempt's output kept in its log file: a runaway agent can't
/// fill the disk. (The pane still shows everything.)
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// Most of the agent's stdout kept in memory to parse for usage.
const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;

/// The file an attempt's output is streamed to: `logs/<id>.<attempt>.log`.
struct StreamedLog {
    file: Option<std::fs::File>,
    written: u64,
}

impl StreamedLog {
    /// A log that can't be created is only a warning: the run goes on, and
    /// the log is captured from the tmux pane at the end instead.
    fn create(path: &Path) -> Self {
        let file = std::fs::File::create(path)
            .map_err(|e| eprintln!("warning: could not write {}: {e}", path.display()))
            .ok();
        Self { file, written: 0 }
    }

    fn write(&mut self, bytes: &[u8]) {
        let Some(file) = &mut self.file else { return };
        let room = MAX_LOG_BYTES.saturating_sub(self.written) as usize;
        let n = bytes.len().min(room);
        let mut ok = file.write_all(&bytes[..n]).is_ok();
        self.written += n as u64;
        if n < bytes.len() && ok {
            ok = file
                .write_all(b"\n[log truncated: size limit reached]\n")
                .is_ok();
            self.written = MAX_LOG_BYTES + 1;
            self.file = None;
        }
        if !ok {
            self.file = None;
        }
    }
}

/// Copies `src` to `dst` (the pane) and the log as it arrives. With
/// `capture`, also returns (a bounded amount of) what it read.
fn tee<R, W>(
    mut src: R,
    mut dst: W,
    log: Arc<Mutex<StreamedLog>>,
    capture: bool,
) -> std::thread::JoinHandle<Vec<u8>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut captured = Vec::new();
        while let Ok(n) = src.read(&mut buf)
            && n > 0
        {
            let _ = dst.write_all(&buf[..n]);
            let _ = dst.flush();
            if let Ok(mut log) = log.lock() {
                log.write(&buf[..n]);
            }
            if capture && captured.len() < MAX_CAPTURE_BYTES {
                captured.extend_from_slice(&buf[..n]);
            }
        }
        captured
    })
}

/// How the agent process ended, as recorded by [`exec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Code(i32),
    Signal(i32),
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Exit::Code(c) => write!(f, "exit {c}"),
            Exit::Signal(s) => write!(f, "signal {s}"),
        }
    }
}

impl std::str::FromStr for Exit {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("unrecognized exit record `{s}`");
        let (kind, n) = s.trim().split_once(' ').ok_or_else(bad)?;
        let n = n.parse().map_err(|_| bad())?;
        match kind {
            "exit" => Ok(Exit::Code(n)),
            "signal" => Ok(Exit::Signal(n)),
            _ => Err(bad()),
        }
    }
}

fn status_file(home: &Home, id: &str, attempt: u32) -> PathBuf {
    home.logs().join(format!("{id}.{attempt}.exit"))
}

fn log_file(home: &Home, id: &str, attempt: u32) -> PathBuf {
    home.logs().join(format!("{id}.{attempt}.log"))
}

fn usage_file(home: &Home, id: &str, attempt: u32) -> PathBuf {
    home.logs().join(format!("{id}.{attempt}.usage"))
}

/// The usage file next to a given `status_file` path (same id and attempt).
fn usage_file_for(status_file: &Path) -> PathBuf {
    status_file.with_extension("usage")
}

/// Token/cost usage an agent reported for one attempt, and how the agent said
/// it went, both parsed from its headless stdout.
///
/// Two shapes are understood, since that is what agents print:
///
/// - one final JSON object with a `usage` field (Claude Code's
///   `--output-format json`);
/// - an event stream, one JSON object per line, where each finished assistant
///   message reports its own usage (pi's `--mode json`). Those are summed, and
///   an error/aborted stop reason is picked up as well: that mode exits 0 even
///   when the run failed.
///
/// Any other agent (plain text, or nothing printed yet) reports nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
}

/// What one attempt's headless stdout said: the usage it reported and, when
/// the stream reports outcomes as events, the error it ended on.
#[derive(Debug, Clone, Default, PartialEq)]
struct Stream {
    usage: Option<Usage>,
    /// Why the agent said the run failed (its own words). `None` unless it
    /// reported one, which for an event stream is not a non-zero exit.
    error: Option<String>,
}

impl Stream {
    fn parse(bytes: &[u8]) -> Stream {
        let single = serde_json::from_slice::<serde_json::Value>(bytes).ok();
        // One object for the whole stdout with a `usage` in it is how Claude
        // Code reports. Anything else that parses as one object is simply a
        // stream of one event, and is read like every other event below.
        if let Some(value) = single.as_ref()
            && let Some(usage) = Usage::from_claude(value)
        {
            return Stream {
                usage: Some(usage),
                error: None,
            };
        }
        let mut stream = Stream::default();
        match single {
            Some(event) => stream.read_event(&event),
            None => {
                // One object per line, as pi's `--mode json` writes them; any
                // other line (a warning, a plain-text agent) is skipped.
                for line in bytes.split(|b| *b == b'\n') {
                    if let Ok(event) = serde_json::from_slice::<serde_json::Value>(line) {
                        stream.read_event(&event);
                    }
                }
            }
        }
        stream
    }

    /// Folds one event of an agent's JSON stream into the totals: the usage of
    /// a finished assistant message, and why it ended if it ended badly.
    fn read_event(&mut self, event: &serde_json::Value) {
        if event.get("type").and_then(serde_json::Value::as_str) != Some("message_end") {
            return; // `agent_end` and friends repeat what `message_end` said
        }
        let Some(message) = event.get("message") else {
            return;
        };
        if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
            return;
        }
        if let Some(usage) = Usage::from_pi(message) {
            self.usage = Some(match self.usage {
                Some(total) => total.add(usage),
                None => usage,
            });
        }
        if let Some(error) = pi_error(message) {
            self.error = Some(error);
        }
    }
}

impl Usage {
    /// Usage as Claude Code reports it: a `usage` object plus a total cost.
    fn from_claude(value: &serde_json::Value) -> Option<Usage> {
        let usage = value.get("usage")?;
        let tokens = |key: &str| usage.get(key).and_then(serde_json::Value::as_u64);
        let tokens_in = tokens("input_tokens").map(|n| {
            n + tokens("cache_creation_input_tokens").unwrap_or(0)
                + tokens("cache_read_input_tokens").unwrap_or(0)
        });
        let tokens_out = tokens("output_tokens");
        let cost_usd = value
            .get("total_cost_usd")
            .or_else(|| value.get("cost_usd"))
            .and_then(serde_json::Value::as_f64);
        Some(Usage {
            tokens_in,
            tokens_out,
            cost_usd,
        })
        .filter(Usage::reported)
    }

    /// Usage as one of pi's assistant messages reports it: `input`, `output`,
    /// the cache counters, and a `cost.total`.
    fn from_pi(message: &serde_json::Value) -> Option<Usage> {
        let usage = message.get("usage")?;
        let count = |key: &str| usage.get(key).and_then(serde_json::Value::as_u64);
        let tokens_in = count("input")
            .map(|n| n + count("cacheRead").unwrap_or(0) + count("cacheWrite").unwrap_or(0));
        let tokens_out = count("output");
        let cost_usd = usage
            .get("cost")
            .and_then(|cost| cost.get("total"))
            .and_then(serde_json::Value::as_f64);
        Some(Usage {
            tokens_in,
            tokens_out,
            cost_usd,
        })
        .filter(Usage::reported)
    }

    /// Did anything in here report a number worth writing down?
    fn reported(&self) -> bool {
        self.tokens_in.is_some() || self.tokens_out.is_some() || self.cost_usd.is_some()
    }

    /// One turn plus another, field by field.
    fn add(self, other: Usage) -> Usage {
        Usage {
            tokens_in: plus(self.tokens_in, other.tokens_in),
            tokens_out: plus(self.tokens_out, other.tokens_out),
            cost_usd: plus(self.cost_usd, other.cost_usd),
        }
    }

    fn to_json(self) -> String {
        serde_json::to_string(&self).expect("Usage always serializes")
    }
}

fn plus<T: std::ops::Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Why pi's assistant message ended badly, if it did: the reason is only in
/// the event stream, since `--mode json` exits 0 either way.
fn pi_error(message: &serde_json::Value) -> Option<String> {
    let stop = message
        .get("stopReason")
        .and_then(serde_json::Value::as_str)?;
    if !matches!(stop, "error" | "aborted") {
        return None;
    }
    let said = message
        .get("errorMessage")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty());
    Some(match said {
        Some(said) => format!("the agent reported {stop}: {said}"),
        None => format!("the agent reported {stop}"),
    })
}

fn read_usage(path: &Path) -> Option<Usage> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn read_exit(path: &Path) -> Result<Option<Exit>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.parse().map_err(anyhow::Error::msg)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// One pass. The home lock is held while task files change, but not while
/// `on_finish` runs (it can talk to the network): successful tasks are
/// collected under the lock and finalized after it's released.
fn tick(home: &Home, config: &Config) -> Result<()> {
    let succeeded = {
        let _lock = home.lock()?;
        let now = now();
        enqueue_schedules(home, config, now)?;
        let succeeded = poll_running(home, config, now)?;
        start_pending(home, config, now)?;
        succeeded
    };
    for (path, task) in succeeded {
        let id = task.id.clone();
        if let Err(e) = finalize(home, config, &path, task) {
            log(&format!("finishing {id}: {e:#}"));
        }
    }
    Ok(())
}

// --- schedules ---------------------------------------------------------------

fn enqueue_schedules(home: &Home, config: &Config, now: DateTime<FixedOffset>) -> Result<()> {
    for path in md_files(&home.schedules())? {
        if let Err(e) = enqueue_schedule(home, config, &path, now) {
            log(&format!("schedule {}: {e:#}", path.display()));
        }
    }
    Ok(())
}

fn enqueue_schedule(
    home: &Home,
    config: &Config,
    path: &Path,
    now: DateTime<FixedOffset>,
) -> Result<()> {
    let mut s = Schedule::load(path)?;
    if !s.enabled {
        return Ok(());
    }
    let Some(last) = s.last_enqueued_at else {
        // First sighting: start counting from now instead of firing for a
        // slot that passed before the schedule existed.
        s.last_enqueued_at = Some(now);
        return s.save(path);
    };
    let slot = s
        .latest_at_or_before(now.with_timezone(&Local))?
        .fixed_offset();
    if slot <= last {
        return Ok(());
    }
    // Record the slot even if enqueueing fails, so a broken schedule logs
    // once per firing instead of once per poll.
    s.last_enqueued_at = Some(slot);
    s.save(path)?;

    let problems = s.problems(config, Some(path));
    if !problems.is_empty() {
        anyhow::bail!("not enqueued: {}", problems.join("; "));
    }
    if !s.allow_overlap && has_queued_run(home, &s.id) {
        log(&format!(
            "schedule {}: skipped {}: the previous run is still queued or running \
             (set `allow_overlap: true` to enqueue anyway)",
            s.id,
            slot.format("%Y-%m-%d %H:%M")
        ));
        return Ok(());
    }
    let streak = failure_streak(home, &s.id);
    if streak >= FAILURE_WARNING_STREAK {
        log(&format!(
            "warning: schedule {}: its last {streak} runs all failed",
            s.id
        ));
    }
    let base = format!("{}-{}", truncate_id(&s.id, 48), slot.format("%Y%m%d-%H%M"));
    let mut task = Task::new(unique_id(home, &base), s.spec.clone(), s.prompt.clone());
    task.created_by = Some(format!("schedule:{}", s.id));
    task.create(&home.tasks().join(format!("{}.md", task.id)))?;
    log(&format!("enqueued {} from schedule {}", task.id, s.id));
    Ok(())
}

/// Consecutive failed runs of a schedule at which the runner starts warning.
const FAILURE_WARNING_STREAK: usize = 3;

fn created_by_schedule(task: &Task, schedule_id: &str) -> bool {
    task.created_by.as_deref() == Some(&format!("schedule:{schedule_id}"))
}

/// Whether a task from this schedule is still in `tasks/` (pending or running).
fn has_queued_run(home: &Home, schedule_id: &str) -> bool {
    md_files(&home.tasks())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| Task::load(p).ok())
        .any(|t| created_by_schedule(&t, schedule_id))
}

/// How many of the schedule's most recent finished runs failed in a row.
pub fn failure_streak(home: &Home, schedule_id: &str) -> usize {
    let mut runs: Vec<Task> = md_files(&home.archive())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| Task::load(p).ok())
        .filter(|t| created_by_schedule(t, schedule_id))
        .collect();
    runs.sort_by_key(|t| std::cmp::Reverse(t.finished_at));
    runs.iter()
        .take_while(|t| t.status == Status::Failed)
        .count()
}

fn truncate_id(id: &str, max: usize) -> &str {
    id[..id.len().min(max)].trim_end_matches('-')
}

// --- running tasks -------------------------------------------------------------

enum Outcome {
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
/// still to run (see [`tick`]).
fn poll_running(
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
fn note_in_log(home: &Home, task: &Task, note: &str) {
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

/// Applies an outcome other than success, which [`poll_running`] hands to
/// [`finalize`] instead.
fn finish(
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
fn finalize(home: &Home, config: &Config, path: &Path, mut task: Task) -> Result<()> {
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
    git::on_finish(eff.on_finish, &dir, &task.id, &task.prompt)
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
fn fail_attempt(
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

// --- starting tasks ------------------------------------------------------------

/// Cost agents reported for tasks run today (local time): finished ones in
/// `archive/` and the ones still queued or running in `tasks/`.
fn spent_today(home: &Home, now: DateTime<FixedOffset>) -> f64 {
    let midnight = now
        .with_timezone(&Local)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(Local).earliest())
        .map(|t| t.fixed_offset());
    let today = |t: &Task| match (midnight, t.finished_at.or(t.started_at)) {
        (Some(m), Some(at)) => at >= m,
        _ => false,
    };
    [home.tasks(), home.archive()]
        .iter()
        .flat_map(|dir| md_files(dir).unwrap_or_default())
        .filter_map(|p| Task::load(&p).ok())
        .filter(today)
        .filter_map(|t| t.cost_usd)
        .sum()
}

static BUDGET_NOTIFIED: AtomicBool = AtomicBool::new(false);

/// Whether `runner.daily_budget_usd` is used up. Logs once per day-crossing.
fn over_budget(home: &Home, config: &Config, now: DateTime<FixedOffset>) -> bool {
    let Some(budget) = config.runner.daily_budget_usd else {
        return false;
    };
    let spent = spent_today(home, now);
    let over = spent >= budget;
    if over && !BUDGET_NOTIFIED.swap(true, Ordering::Relaxed) {
        log(&format!(
            "daily budget reached (${spent:.2} of ${budget:.2}): not starting new tasks until tomorrow"
        ));
    } else if !over {
        BUDGET_NOTIFIED.store(false, Ordering::Relaxed);
    }
    over
}

fn start_pending(home: &Home, config: &Config, now: DateTime<FixedOffset>) -> Result<()> {
    if over_budget(home, config, now) {
        return Ok(());
    }
    let mut per_repo: HashMap<PathBuf, usize> = HashMap::new();
    let running = load_tasks(home, Status::Running)?;
    for (_, t) in &running {
        *per_repo
            .entry(t.spec.resolve(&config.defaults).repo)
            .or_default() += 1;
    }
    let mut total = running.len();

    let mut pending: Vec<_> = load_tasks(home, Status::Pending)?
        .into_iter()
        .filter(|(_, t)| t.scheduled_at.is_none_or(|at| at <= now))
        .collect();
    pending.sort_by_key(|(_, t)| (t.scheduled_at.or(t.created_at), t.id.clone()));

    for (path, task) in pending {
        if total >= config.runner.max_parallel {
            break;
        }
        let repo = task.spec.resolve(&config.defaults).repo;
        let in_repo = per_repo.entry(repo).or_default();
        if *in_repo >= config.runner.max_parallel_per_repo {
            continue;
        }
        match start(home, config, &path, task, now) {
            Ok(true) => {
                total += 1;
                *in_repo += 1;
            }
            Ok(false) => {}
            Err(e) => log(&format!("starting {}: {e:#}", path.display())),
        }
    }
    Ok(())
}

/// Starts one attempt. Returns whether the task is now running.
fn start(
    home: &Home,
    config: &Config,
    path: &Path,
    mut task: Task,
    now: DateTime<FixedOffset>,
) -> Result<bool> {
    let eff = task.spec.resolve(&config.defaults);
    let problems = start_problems(config, &task, path);
    if !problems.is_empty() {
        task.status = Status::Failed;
        task.error = Some(format!("invalid task: {}", problems.join("; ")));
        archive(home, path, task, now)?;
        return Ok(false);
    }

    task.attempts += 1;
    task.started_at = Some(now);
    let session = &config.runner.tmux_session;
    let launched = prepare_workdir(home, config, &task, &eff).and_then(|dir| {
        // A leftover window with the same name would make targets ambiguous.
        tmux::kill_window(session, &task.id)?;
        // A record left by an earlier task with this id would end this
        // attempt as soon as it's polled.
        let status = status_file(home, &task.id, task.attempts);
        remove_if_exists(&status)?;
        remove_if_exists(&log_file(home, &task.id, task.attempts))?;
        tmux::spawn(
            session,
            &task.id,
            &dir,
            &exec_argv(home, &task.id, &status)?,
        )
    });
    match launched {
        Ok(()) => {
            task.status = Status::Running;
            task.tmux_window = Some(format!("{session}:{}", task.id));
            task.save(path)?;
            log(&format!(
                "started {} (attempt {}) in tmux {session}:{}",
                task.id, task.attempts, task.id
            ));
            Ok(true)
        }
        Err(e) => {
            fail_attempt(
                home,
                path,
                task,
                &eff,
                format!("could not start: {e:#}"),
                true,
                now,
            )?;
            Ok(false)
        }
    }
}

/// Everything that stops a task from starting: `Task::problems` plus
/// runner-only checks. Chiefly, whether the agent's program can actually be
/// run — that is deliberately not part of `Task::problems`, so a task may
/// still be queued (and validated) before its agent is installed.
fn start_problems(config: &Config, task: &Task, path: &Path) -> Vec<String> {
    let mut out = task.problems(config, Some(path));
    let eff = task.spec.resolve(&config.defaults);
    if let Some(name) = eff.agent.as_deref()
        && let Some(agent) = config.agents.get(name)
    {
        let template = match eff.mode {
            Mode::Headless => &agent.headless,
            Mode::Interactive => &agent.interactive,
        };
        if let Some(program) = template.first()
            && !crate::config::on_path(program)
            && !Path::new(program).is_file()
        {
            out.push(format!(
                "agent `{name}`: `{program}` is not on $PATH; install it or fix \
                 [agents.{name}] in config.toml (`furmi doctor` shows the setup)"
            ));
        }
    }
    out
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

pub fn workdir(home: &Home, task: &Task, eff: &Effective) -> PathBuf {
    if eff.worktree {
        home.worktrees().join(&task.id)
    } else {
        eff.repo.clone()
    }
}

fn prepare_workdir(home: &Home, config: &Config, task: &Task, eff: &Effective) -> Result<PathBuf> {
    let dir = workdir(home, task, eff);
    match task.effective_branch(config) {
        Some(branch) if eff.worktree => git::ensure_worktree(&eff.repo, &dir, &branch)?,
        Some(branch) => git::checkout_branch(&eff.repo, &branch)?,
        None => {}
    }
    Ok(dir)
}

fn exec_argv(home: &Home, id: &str, status_file: &Path) -> Result<Vec<String>> {
    let exe = crate::exe::path().context("locating the furmi binary")?;
    let path = |p: &Path| {
        p.to_str()
            .map(String::from)
            .context("path is not valid UTF-8")
    };
    Ok(vec![
        path(&exe)?,
        "--home".into(),
        path(home.root())?,
        "exec".into(),
        id.into(),
        "--status-file".into(),
        path(status_file)?,
    ])
}

/// The agent's argv for `mode`, with `model_args` appended if there's a
/// model. `{prompt}` is filled last so placeholders inside the prompt text
/// stay literal.
fn agent_argv(
    agent: &Agent,
    mode: Mode,
    prompt: &str,
    model: Option<&str>,
    id: &str,
) -> Vec<String> {
    let template = match mode {
        Mode::Headless => &agent.headless,
        Mode::Interactive => &agent.interactive,
    };
    let model_args = match model {
        Some(_) => agent.model_args.as_slice(),
        None => &[],
    };
    template
        .iter()
        .chain(model_args)
        .map(|a| {
            a.replace("{model}", model.unwrap_or_default())
                .replace("{id}", id)
                .replace("{prompt}", prompt)
        })
        .collect()
}

// --- helpers -------------------------------------------------------------------

fn load_tasks(home: &Home, status: Status) -> Result<Vec<(PathBuf, Task)>> {
    let mut out = Vec::new();
    for path in md_files(&home.tasks())? {
        match Task::load(&path) {
            Ok(t) if t.status == status => out.push((path, t)),
            Ok(_) => {}
            Err(e) => log(&format!("skipping {}: {e:#}", path.display())),
        }
    }
    Ok(out)
}

/// Where [`log`] also appends, once `furmi run` has set it.
static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
/// The log file is moved to `.1` (replacing an older one) beyond this size.
const MAX_RUNNER_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub fn log(msg: &str) {
    let line = format!("{} {msg}", Local::now().format("%Y-%m-%d %H:%M:%S"));
    println!("{line}");
    if let Some(path) = LOG_FILE.get() {
        append_log_line(path, &line);
    }
}

fn append_log_line(path: &Path, line: &str) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_RUNNER_LOG_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_agent_placeholders() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let agent = Agent {
            headless: argv(&["agent", "run", "{prompt}", "x-{id}"]),
            interactive: argv(&["agent", "{prompt}"]),
            model_args: argv(&["-m", "{model}"]),
            model: None,
        };
        let prompt = "fix {model}; rm -rf";
        assert_eq!(
            agent_argv(&agent, Mode::Headless, prompt, Some("big"), "t1"),
            argv(&["agent", "run", prompt, "x-t1", "-m", "big"])
        );
        assert_eq!(
            agent_argv(&agent, Mode::Interactive, prompt, None, "t1"),
            argv(&["agent", prompt])
        );
    }

    #[test]
    fn retries_count_extra_attempts() {
        assert!(retry_left(1, 1));
        assert!(!retry_left(2, 1));
        assert!(!retry_left(1, 0));
    }

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

    #[test]
    fn exit_records_roundtrip() {
        for e in [Exit::Code(0), Exit::Code(127), Exit::Signal(15)] {
            assert_eq!(e.to_string().parse::<Exit>(), Ok(e));
        }
        assert!("garbage".parse::<Exit>().is_err());
    }

    #[test]
    fn truncates_ids_without_trailing_dash() {
        assert_eq!(truncate_id("nightly-deps", 48), "nightly-deps");
        assert_eq!(truncate_id("abc-def", 4), "abc");
    }

    #[test]
    fn parses_claude_code_usage() {
        let stdout = r#"{"type":"result","total_cost_usd":0.0512,"usage":{"input_tokens":120,"cache_creation_input_tokens":30,"cache_read_input_tokens":500,"output_tokens":80}}"#;
        let stream = Stream::parse(stdout.as_bytes());
        let usage = stream.usage.unwrap();
        assert_eq!(usage.tokens_in, Some(120 + 30 + 500));
        assert_eq!(usage.tokens_out, Some(80));
        assert_eq!(usage.cost_usd, Some(0.0512));
        assert_eq!(stream.error, None);
    }

    #[test]
    fn parses_pi_json_events_by_summing_turns() {
        // Two turns, each reporting its own usage; the surrounding events
        // (and a non-JSON line) must not add anything or trip it up.
        let stdout = [
            r#"{"type":"session","version":3,"id":"s"}"#,
            r#"{"type":"agent_start"}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"toolUse","usage":{"input":100,"output":10,"cacheRead":5,"cacheWrite":2,"totalTokens":117,"cost":{"total":0.01}}}}"#,
            "warning: something on stderr-ish stdout",
            r#"{"type":"message_end","message":{"role":"user","content":"and then?"}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop","usage":{"input":200,"output":20,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.02}}}}"#,
            // agent_end repeats the same messages; counting those would double.
            r#"{"type":"agent_end","messages":[{"role":"assistant","usage":{"input":100,"output":10}}]}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]
        .join("\n");
        let stream = Stream::parse(stdout.as_bytes());
        let usage = stream.usage.unwrap();
        assert_eq!(usage.tokens_in, Some((100 + 5 + 2) + 200));
        assert_eq!(usage.tokens_out, Some(30));
        assert_eq!(usage.cost_usd, Some(0.03));
        assert_eq!(stream.error, None);
    }

    #[test]
    fn a_pi_stream_error_is_reported_even_though_the_exit_was_zero() {
        // pi's `--mode json` exits 0 for an aborted or errored response, so
        // the reason has to come out of the stream.
        let stdout = [
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"model is not available","usage":{"input":10,"output":0,"cost":{"total":0.001}}}}"#,
        ]
        .join("\n");
        let stream = Stream::parse(stdout.as_bytes());
        assert_eq!(
            stream.error.as_deref(),
            Some("the agent reported error: model is not available")
        );
        // The failed turn still reports what it spent.
        assert_eq!(stream.usage.unwrap().tokens_in, Some(10));

        // An abort with nothing said about why.
        let aborted =
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"aborted"}}"#;
        assert_eq!(
            Stream::parse(aborted.as_bytes()).error.as_deref(),
            Some("the agent reported aborted")
        );

        // A stream that ends normally says nothing about failing.
        let ok = r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}"#;
        let stream = Stream::parse(ok.as_bytes());
        assert_eq!(stream.error, None);
        assert_eq!(stream.usage, None);
    }

    #[test]
    fn no_usage_for_non_json_or_usage_less_output() {
        assert_eq!(
            Stream::parse(b"plain text output, not json"),
            Stream::default()
        );
        assert_eq!(
            Stream::parse(br#"{"type":"result","result":"ok"}"#),
            Stream::default()
        );
    }

    #[test]
    fn usage_roundtrips_through_json() {
        let usage = Usage {
            tokens_in: Some(650),
            tokens_out: Some(80),
            cost_usd: Some(0.0512),
        };
        let read_back: Usage = serde_json::from_str(&usage.to_json()).unwrap();
        assert_eq!(usage, read_back);
    }
}
