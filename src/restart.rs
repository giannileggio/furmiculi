//! `furmi restart`: stop the dashboard and the runner and start them again the
//! way they were started, so they run the binary that is installed now.
//!
//! Tasks run in tmux windows the runner doesn't own, and the next runner
//! re-adopts them, so a restart is graceful for tasks. Under `furmi service`
//! the service manager owns the processes: it is asked to restart them, since
//! starting a second copy beside the managed one would only fight it.

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::control;
use crate::home::{Dashboard, Home, Launch};
use crate::web;

/// Unit (systemd) and label (launchd) names `furmi service` prints.
const RUNNER_UNIT: &str = "furmiculi";
const DASHBOARD_UNIT: &str = "furmiculi-web";
const RUNNER_LABEL: &str = "dev.furmiculi";
const DASHBOARD_LABEL: &str = "dev.furmiculi-web";

/// How long a managed process gets to come back after the manager restarts it.
const SERVICE_WAIT: Duration = Duration::from_secs(15);

pub fn restart(home: &Home) -> Result<()> {
    let dashboard = home.dashboard_info();
    let runner = home.runner_pid();
    if dashboard.is_none() && runner.is_none() {
        println!("Nothing is running; `furmi start` starts the dashboard and the runner.");
        return Ok(());
    }
    let dashboard_unit = dashboard
        .as_ref()
        .and_then(|d| Manager::native().owning(DASHBOARD_UNIT, DASHBOARD_LABEL, d.pid));
    let runner_unit =
        runner.and_then(|pid| Manager::native().owning(RUNNER_UNIT, RUNNER_LABEL, pid));

    // Everything that could stop us happens before anything is stopped: a
    // restart that kills the dashboard and then can't start it is worse than
    // no restart.
    let relaunch = match (&dashboard, &dashboard_unit) {
        (Some(d), None) => Some(launch_of(d)?),
        _ => None,
    };

    if let (Some(d), Some(manager)) = (&dashboard, &dashboard_unit) {
        manager.restart(DASHBOARD_UNIT, DASHBOARD_LABEL)?;
        let up = control::wait_for(SERVICE_WAIT, || {
            home.dashboard().is_some_and(|(pid, _)| pid != d.pid)
        });
        report(
            up,
            "dashboard",
            manager.name(),
            home.dashboard().map(|(p, _)| p),
        );
    }
    // Stop both before starting either, so a runner begun by the old dashboard
    // is gone before the new dashboard decides whether it needs one.
    if let (Some(d), Some(_)) = (&dashboard, &relaunch) {
        // A `furmi start` dashboard takes the runner it began down with it.
        let pid = control::stop_dashboard(home)?.unwrap_or(d.pid);
        println!("Stopped the dashboard (pid {pid}).");
    }
    let runner_by_hand = runner.is_some() && runner_unit.is_none();
    if runner_by_hand
        && home.runner_alive()
        && let Some(pid) = control::stop_runner(home)?
    {
        println!("Stopped the runner (pid {pid}). Tasks already running keep going.");
    }
    if let Some(launch) = &relaunch {
        web::relaunch(home, launch)?;
    }
    if runner_by_hand && control::start_runner(home)? {
        println!("Runner started.");
    }
    if let (Some(old), Some(manager)) = (runner, &runner_unit) {
        manager.restart(RUNNER_UNIT, RUNNER_LABEL)?;
        let up = control::wait_for(SERVICE_WAIT, || {
            home.runner_pid().is_some_and(|pid| pid != old)
        });
        report(up, "runner", manager.name(), home.runner_pid());
    }
    Ok(())
}

fn report(up: bool, what: &str, manager: &str, pid: Option<u32>) {
    match (up, pid) {
        (true, Some(pid)) => println!("Restarted the {what} through {manager} (now pid {pid})."),
        _ => println!(
            "Asked {manager} to restart the {what}, but it hasn't come back yet; \
             see its log for why."
        ),
    }
}

/// How to start this dashboard again, or why furmi can't.
fn launch_of(dashboard: &Dashboard) -> Result<Launch> {
    let Some(launch) = &dashboard.launch else {
        bail!(
            "the dashboard (pid {}) was started by a furmi that didn't record how, so it can't \
             be started the same way. Stop it with `furmi stop` and start it again with \
             `furmi start -d` (or `furmi web -d`); later restarts will work.",
            dashboard.pid
        );
    };
    if launch.inline_token {
        bail!(
            "the dashboard (pid {}) was started with `--token`, which furmi doesn't keep, so it \
             can't be restarted for you. Stop it with `furmi stop` and start it again; \
             `--token-file` (or `furmi web --setup`) lets `furmi restart` work.",
            dashboard.pid
        );
    }
    Ok(launch.clone())
}

/// What supervises `furmi service` units on this machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    fn native() -> Self {
        if cfg!(target_os = "macos") {
            Manager::Launchd
        } else {
            Manager::Systemd
        }
    }

    fn name(self) -> &'static str {
        match self {
            Manager::Systemd => "systemd",
            Manager::Launchd => "launchd",
        }
    }

    /// `self` if it supervises the process `pid` as `unit`/`label`: a process
    /// somebody started by hand under the same name isn't the service's.
    fn owning(self, unit: &str, label: &str, pid: u32) -> Option<Self> {
        let out = match self {
            Manager::Systemd => command_output(
                "systemctl",
                &["--user", "show", "-p", "MainPID", "--value", unit],
            )?,
            Manager::Launchd => command_output("launchctl", &["list", label])?,
        };
        let main = match self {
            Manager::Systemd => parse_main_pid(&out),
            Manager::Launchd => parse_launchctl_pid(&out),
        };
        (main == Some(pid)).then_some(self)
    }

    fn restart(self, unit: &str, label: &str) -> Result<()> {
        let (program, args): (&str, Vec<String>) = match self {
            Manager::Systemd => (
                "systemctl",
                vec!["--user".into(), "restart".into(), unit.into()],
            ),
            Manager::Launchd => (
                "launchctl",
                vec![
                    "kickstart".into(),
                    "-k".into(),
                    // SAFETY: getuid(2) cannot fail.
                    format!("gui/{}/{label}", unsafe { libc::getuid() }),
                ],
            ),
        };
        let status = Command::new(program)
            .args(&args)
            .status()
            .with_context(|| format!("running {program}"))?;
        if !status.success() {
            bail!("`{program} {}` failed ({status})", args.join(" "));
        }
        Ok(())
    }
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `systemctl show -p MainPID --value`: the pid, or 0 for a stopped unit.
fn parse_main_pid(out: &str) -> Option<u32> {
    out.trim().parse().ok().filter(|&p| p != 0)
}

/// `launchctl list <label>`: a `"PID" = 123;` line while it runs.
fn parse_launchctl_pid(out: &str) -> Option<u32> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("\"PID\" = "))
        .and_then(|rest| rest.trim_end_matches(';').trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_managers_answers_are_read_for_the_pid() {
        assert_eq!(parse_main_pid("1234\n"), Some(1234));
        assert_eq!(parse_main_pid("0\n"), None, "a stopped unit");
        assert_eq!(parse_main_pid(""), None);
        let listed =
            "{\n\t\"Label\" = \"dev.furmiculi\";\n\t\"PID\" = 321;\n\t\"Program\" = \"x\";\n};";
        assert_eq!(parse_launchctl_pid(listed), Some(321));
        assert_eq!(parse_launchctl_pid("{\n\t\"Label\" = \"x\";\n};"), None);
    }

    fn dashboard(launch: Option<Launch>) -> Dashboard {
        Dashboard {
            pid: 7,
            addr: "127.0.0.1:7433".into(),
            version: Some("0.4.0".into()),
            launch,
        }
    }

    #[test]
    fn a_dashboard_that_cant_be_started_again_is_refused_before_anything_stops() {
        let old = launch_of(&dashboard(None)).unwrap_err().to_string();
        assert!(old.contains("didn't record how"), "{old}");

        let inline = launch_of(&dashboard(Some(Launch {
            inline_token: true,
            ..Launch::default()
        })))
        .unwrap_err()
        .to_string();
        assert!(inline.contains("--token-file"), "{inline}");

        let flags = Launch {
            port: 7500,
            ..Launch::default()
        };
        assert_eq!(launch_of(&dashboard(Some(flags.clone()))).unwrap(), flags);
    }
}
