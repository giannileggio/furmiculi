//! `furmi exec`: what a task's tmux window runs. Starts the agent, streams
//! and parses its headless output, and records how it ended for the runner to
//! reap.

use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{Agent, Config};
use crate::fsutil;
use crate::home::Home;
use crate::spec::Mode;
use crate::task::Task;

use super::usage::Stream;

/// Runs inside the task's tmux window: runs the agent, then writes how it
/// ended to `status_file` for `poll_running` to pick up. A headless
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
            // The child exiting is what decides the run. The tees only add
            // what it printed: a process it left behind holding the pipes
            // open must not keep the exit record from being written.
            let captured = tees.and_then(collect_tees);
            let exit = match (status.code(), status.signal()) {
                (Some(code), _) => Exit::Code(code),
                (None, Some(sig)) => Exit::Signal(sig),
                (None, None) => Exit::Code(1),
            };
            let stream = captured.map(|out| Stream::parse(&out)).unwrap_or_default();
            let exit = recorded_exit(exit, &stream);
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
/// `capture`, also collects (a bounded amount of) what it read, readable at
/// any time rather than only once the stream reaches EOF.
fn tee<R, W>(mut src: R, mut dst: W, log: Arc<Mutex<StreamedLog>>, capture: bool) -> Tee
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let (tx, done) = mpsc::channel();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let tee_captured = captured.clone();
    let handle = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = src.read(&mut buf)
            && n > 0
        {
            let _ = dst.write_all(&buf[..n]);
            let _ = dst.flush();
            if let Ok(mut log) = log.lock() {
                log.write(&buf[..n]);
            }
            if capture
                && let Ok(mut captured) = tee_captured.lock()
                && captured.len() < MAX_CAPTURE_BYTES
            {
                let room = MAX_CAPTURE_BYTES - captured.len();
                captured.extend_from_slice(&buf[..n.min(room)]);
            }
        }
        // The receiver may be gone (an abandoned tee): that's fine.
        let _ = tx.send(());
    });
    Tee {
        is_stdout: capture,
        captured,
        done,
        handle,
    }
}

/// A tee thread: what it captured so far is readable at any time, and `done`
/// fires when the stream reaches EOF (never, if a leaked child keeps it open).
struct Tee {
    is_stdout: bool,
    captured: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
    handle: std::thread::JoinHandle<()>,
}

/// How long the tees get, in total, to reach EOF after the agent exited.
/// Output a process the agent left behind still holds the pipe for is not
/// waited for; whatever was captured by then is used. A leaked child must
/// cost at most a tail of the log, never the run's outcome.
const TEE_DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Returns what the stdout tee captured, waiting at most
/// [`TEE_DRAIN_GRACE`] for the tees to reach EOF. A tee still blocked on a
/// pipe a leaked child holds open is abandoned after that: its thread dies
/// with this process.
fn collect_tees((out, err): (Tee, Tee)) -> Option<Vec<u8>> {
    let deadline = Instant::now() + TEE_DRAIN_GRACE;
    let mut captured = None;
    for tee in [out, err] {
        let left = deadline.saturating_duration_since(Instant::now());
        if tee.done.recv_timeout(left).is_ok() {
            let _ = tee.handle.join();
        }
        if tee.is_stdout {
            captured = Some(tee.captured.lock().map(|b| b.clone()).unwrap_or_default());
        }
    }
    captured
}

/// How the agent process ended, as recorded by [`exec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Exit {
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

/// The exit to record for a run: pi's `--mode json` exits 0 even when the
/// run failed, so a stream that ended on an error turns that 0 into a
/// failure. Anything else (a non-zero exit) already speaks for itself.
pub(super) fn recorded_exit(exit: Exit, stream: &Stream) -> Exit {
    match (exit, &stream.error) {
        (Exit::Code(0), Some(error)) => {
            eprintln!("error: {error}");
            Exit::Code(1)
        }
        (exit, _) => exit,
    }
}

pub(super) fn read_exit(path: &Path) -> Result<Option<Exit>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.parse().map_err(anyhow::Error::msg)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The usage file next to a given `status_file` path (same id and attempt).
fn usage_file_for(status_file: &Path) -> PathBuf {
    status_file.with_extension("usage")
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
    fn exit_records_roundtrip() {
        for e in [Exit::Code(0), Exit::Code(127), Exit::Signal(15)] {
            assert_eq!(e.to_string().parse::<Exit>(), Ok(e));
        }
        assert!("garbage".parse::<Exit>().is_err());
    }
    #[test]
    fn a_leaked_child_does_not_delay_the_outcome() {
        // The agent prints its final answer and exits, but leaves a child
        // behind that still holds stdout and stderr open, so the tees never
        // see EOF. What was captured must still come back, without waiting.
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("printf '{\"type\":\"agent_settled\",\"aborted\":false}\\n'; sleep 3 & exit 0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let log = Arc::new(Mutex::new(StreamedLog::create(&dir.path().join("t.log"))));
        let tees = (
            tee(
                child.stdout.take().unwrap(),
                std::io::sink(),
                log.clone(),
                true,
            ),
            tee(child.stderr.take().unwrap(), std::io::sink(), log, false),
        );
        child.wait().unwrap();

        let started = Instant::now();
        let captured = collect_tees(tees).expect("the stdout tail was still captured");
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(2),
            "waited {waited:?} for a child the agent left behind"
        );
        let text = String::from_utf8_lossy(&captured);
        assert!(text.contains(r#""agent_settled""#), "{text}");
    }
}
