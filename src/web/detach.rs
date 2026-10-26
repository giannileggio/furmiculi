//! `furmi web -d` and `furmi start -d`: serving in the background, in its own
//! process group with its output in web.log, and saying here what the child
//! would otherwise print to a terminal it doesn't have.

use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::bind::same_port;
use super::open_browser;
use super::setup::{Serving, WebArgs, print_remote};

use crate::control;
use crate::home::{Home, contract_tilde};

/// How long a backgrounded dashboard gets to record itself before `furmi
/// start -d` calls the spawn a failure.
const DETACH_WAIT: Duration = Duration::from_secs(10);

/// Serves the dashboard (`with_runner`, for `furmi start`: the runner too) in
/// its own process group with its output in `web.log`, then returns — so the
/// shell that started it gets its prompt back and closing it changes nothing.
/// Everything worth reading is printed here, since the child has no terminal.
pub(super) fn detach(
    home: &Home,
    with_runner: bool,
    args: WebArgs,
    serving: &Serving,
) -> Result<()> {
    if let Some((pid, served)) = home
        .dashboard()
        .filter(|(_, served)| same_port(served, &serving.addr))
        && !args.replace
    {
        println!(
            "A dashboard is already serving http://{served} (pid {pid}); \
             `furmi stop` stops it, `--replace` starts a new one."
        );
        // That dashboard may be `furmi web`'s, or one whose runner was
        // stopped since — either way the runner is the half `start` is here
        // for, and no child of ours is going to spawn it.
        if with_runner {
            start_runner_here(home);
        }
        return Ok(());
    }
    let exe = crate::exe::path().context("finding the furmi binary to background the dashboard")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.web_log())
        .with_context(|| format!("opening {}", home.web_log().display()))?;
    let errors = log.try_clone().context("duplicating the dashboard log")?;
    let mut child = Command::new(exe)
        .arg("--home")
        .arg(home.root())
        .arg(if with_runner { "start" } else { "web" })
        .args(child_args(&args))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errors))
        // Its own process group: the shell that started it HUPs its own jobs
        // when it closes, and this isn't one of them.
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting the dashboard on {}", serving.addr))?;
    // Wait for *this* child to record itself (which happens once it's really
    // serving), so a bad bind is reported here instead of in a log file — and
    // so a dashboard it's replacing doesn't answer for it.
    let pid = child.id();
    let mut exited = None;
    let up = control::wait_for(DETACH_WAIT, || {
        exited = child.try_wait().ok().flatten();
        home.dashboard().is_some_and(|(serving, _)| serving == pid) || exited.is_some()
    });
    if let Some(status) = exited {
        bail!(
            "the dashboard exited right away ({status}){}; see {}",
            last_log_line(home)
                .map(|l| format!(": {l}"))
                .unwrap_or_default(),
            contract_tilde(&home.web_log())
        );
    }
    if !up {
        let _ = child.kill();
        bail!(
            "the dashboard didn't come up within {}s; see {}",
            DETACH_WAIT.as_secs(),
            contract_tilde(&home.web_log())
        );
    }
    // Reap it when it eventually exits, instead of leaving a zombie behind.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let addr = home
        .dashboard()
        .map(|(_, addr)| addr)
        .unwrap_or_else(|| serving.addr.clone());
    let tty = std::io::stdout().is_terminal();
    println!("Dashboard: http://{addr}  (in the background; `furmi stop` stops it)");
    if serving.remote {
        print_remote(serving, serving.bearer.as_deref().unwrap_or_default(), tty);
    }
    if with_runner {
        // The child spawns the runner just before it binds; give its lock a
        // moment to appear so the answer here is the real one.
        let started = control::wait_for(Duration::from_secs(5), || home.runner_alive());
        if started {
            println!(
                "Runner started (it logs to {})",
                contract_tilde(&home.runner_log())
            );
        } else {
            println!(
                "The runner hasn't come up yet; if it doesn't, see {}",
                contract_tilde(&home.runner_log())
            );
        }
    }
    if !args.no_open && !serving.remote && tty {
        open_browser(&format!("http://{addr}"));
    }
    Ok(())
}

/// Starts the runner for a `furmi start -d` that found a dashboard already
/// serving, and says what happened: this is the process with the terminal, and
/// the backgrounded child that would normally start the runner never ran.
fn start_runner_here(home: &Home) {
    let log = contract_tilde(&home.runner_log());
    match control::start_runner(home) {
        Ok(true) => println!("Runner started (it logs to {log})"),
        Ok(false) => println!("The runner was already running (it logs to {log})."),
        Err(e) => println!(
            "The dashboard is serving, but the runner didn't start: {e:#}; \
             see {log}"
        ),
    }
}

/// The last thing a backgrounded dashboard logged, for reporting why it
/// didn't come up (a busy address, usually) without a trip to the file.
fn last_log_line(home: &Home) -> Option<String> {
    let text = std::fs::read_to_string(home.web_log()).ok()?;
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    Some(line.trim().trim_start_matches("error: ").to_string())
}

/// The command line the backgrounded child needs to serve where this process
/// decided: its flags, minus `--detach` (it would fork again), `--setup` (the
/// answers are in config.toml by now, and it has no terminal to ask on) and
/// the browser (this process opens it: the child would see no tty).
fn child_args(args: &WebArgs) -> Vec<std::ffi::OsString> {
    let mut argv: Vec<std::ffi::OsString> = vec![
        "--port".into(),
        args.port.to_string().into(),
        "--no-open".into(),
    ];
    if let Some(listen) = &args.listen {
        argv.push("--listen".into());
        argv.push(listen.into());
    }
    if let Some(file) = &args.token_file {
        argv.push("--token-file".into());
        argv.push(file.as_os_str().to_owned());
    }
    if let Some(token) = &args.token {
        argv.push("--token".into());
        argv.push(token.into());
    }
    for host in &args.allowed_hosts {
        argv.push("--allowed-host".into());
        argv.push(host.into());
    }
    if args.replace {
        argv.push("--replace".into());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_backgrounded_child_gets_the_flags_minus_the_loops() {
        let args = WebArgs {
            port: 7500,
            listen: Some("0.0.0.0:7500".into()),
            token_file: Some(PathBuf::from("/tmp/tok")),
            token: None,
            allowed_hosts: vec!["box.ts.net".into()],
            setup: true,
            replace: true,
            no_open: false,
            detach: true,
        };
        let argv: Vec<String> = child_args(&args)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // `--detach` would fork again and `--setup` would ask a terminal-less
        // child questions; `--no-open` is this process's job now.
        assert_eq!(
            argv,
            [
                "--port",
                "7500",
                "--no-open",
                "--listen",
                "0.0.0.0:7500",
                "--token-file",
                "/tmp/tok",
                "--allowed-host",
                "box.ts.net",
                "--replace",
            ]
        );
    }
}
