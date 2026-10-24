//! `furmi status`, `furmi doctor` and `furmi service`: is the runner alive, is the
//! machine set up to run tasks, and how to keep the runner running.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::Local;

use crate::cmd::md_files;
use crate::config::{Config, on_path};
use crate::frontmatter;
use crate::home::Home;
use crate::run::{self, RunnerState};
use crate::schedule::Schedule;
use crate::task::{Status, Task, fmt_cost, now};

/// A pass this many poll intervals late means the runner is wedged.
const STALE_AFTER_POLLS: u32 = 4;

/// What's going on right now: runner liveness, queue counts, budget, and
/// schedules that keep failing.
pub fn status(home: &Home) -> Result<()> {
    home.ensure_initialized()?;
    let config = Config::load(&home.config_path())?;

    let state = RunnerState::load(home);
    if home.runner_alive() {
        let pid = state
            .as_ref()
            .map(|s| s.pid.to_string())
            .unwrap_or_default();
        println!("runner     running (pid {pid})");
        if let Some(last) = state.as_ref().and_then(|s| s.last_tick_at) {
            let age = (now() - last).to_std().unwrap_or_default();
            println!(
                "           last pass {} ago",
                humantime::format_duration(age)
            );
            if age > config.runner.poll_interval * STALE_AFTER_POLLS {
                println!(
                    "           WARNING: polling every {}, so the runner looks stuck (see {})",
                    humantime::format_duration(config.runner.poll_interval),
                    home.runner_log().display()
                );
            }
        }
        if let Some(err) = state.and_then(|s| s.last_error) {
            println!("           last pass failed: {err}");
        }
    } else {
        println!(
            "runner     NOT running: start it with `furmi start` (or `furmi run`; `furmi service` keeps it up)"
        );
    }

    let load = |dir: std::path::PathBuf| -> Vec<Task> {
        md_files(&dir)
            .unwrap_or_default()
            .iter()
            .filter_map(|p| Task::load(p).ok())
            .collect()
    };
    let queued = load(home.tasks());
    let archived = load(home.archive());
    let count = |tasks: &[Task], status| tasks.iter().filter(|t| t.status == status).count();
    println!(
        "queue      {} pending, {} running",
        count(&queued, Status::Pending),
        count(&queued, Status::Running)
    );
    println!(
        "archive    {} done, {} failed, {} cancelled, {} needs review",
        count(&archived, Status::Done),
        count(&archived, Status::Failed),
        count(&archived, Status::Cancelled),
        count(&archived, Status::NeedsReview)
    );

    if let Some(budget) = config.runner.daily_budget_usd {
        let cost = |tasks: &[Task]| {
            let midnight = now()
                .with_timezone(&Local)
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|t| t.and_local_timezone(Local).earliest());
            tasks
                .iter()
                .filter(|t| {
                    midnight
                        .zip(t.finished_at.or(t.started_at))
                        .is_some_and(|(m, at)| at >= m)
                })
                .filter_map(|t| t.cost_usd)
                .sum::<f64>()
        };
        let spent = cost(&queued) + cost(&archived);
        println!(
            "budget     {} of {} spent today",
            fmt_cost(Some(spent)),
            fmt_cost(Some(budget))
        );
    }

    for path in md_files(&home.schedules())? {
        let Ok(s) = Schedule::load(&path) else {
            continue;
        };
        let streak = run::failure_streak(home, &s.id);
        if streak >= 2 {
            println!("schedule   {}: its last {streak} runs failed", s.id);
        }
    }
    Ok(())
}

pub enum Check {
    Ok(String),
    Warn(String),
    Fail(String),
}

impl Check {
    /// `ok`, `warn` or `fail`.
    pub fn level(&self) -> &'static str {
        match self {
            Check::Ok(_) => "ok",
            Check::Warn(_) => "warn",
            Check::Fail(_) => "fail",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Check::Ok(m) | Check::Warn(m) | Check::Fail(m) => m,
        }
    }
}

/// Checks that tasks can actually run here. Exits non-zero if anything is
/// wrong enough to stop them.
pub fn doctor(home: &Home) -> Result<ExitCode> {
    let mut checks = checks(home);
    checks.push(runner_check(home));
    report(checks)
}

fn runner_check(home: &Home) -> Check {
    if home.runner_alive() {
        Check::Ok("a runner is running".into())
    } else {
        Check::Warn("no runner is running: start it with `furmi start` (or `furmi run`)".into())
    }
}

/// Everything `furmi doctor` checks except whether a runner is up (see
/// [`runner_check`]): the tools, config, agents and task files. The
/// dashboard shows these as its setup check.
pub fn checks(home: &Home) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut add = |c: Check| checks.push(c);

    for tool in ["tmux", "git"] {
        if on_path(tool) {
            add(Check::Ok(format!("{tool} is installed")));
        } else {
            add(Check::Fail(format!(
                "{tool} is not on $PATH: `furmi run` needs it"
            )));
        }
    }

    if home.ensure_initialized().is_err() {
        add(Check::Fail(format!(
            "{} is not initialized: run `furmi init`",
            home.root().display()
        )));
        return checks;
    }
    let probe = home.root().join(".doctor-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            add(Check::Ok(format!("{} is writable", home.root().display())));
        }
        Err(e) => add(Check::Fail(format!(
            "{} is not writable: {e}",
            home.root().display()
        ))),
    }

    let config = match Config::load(&home.config_path()) {
        Ok(c) => {
            add(Check::Ok("config.toml is valid".into()));
            c
        }
        Err(e) => {
            add(Check::Fail(format!("{e:#}")));
            return checks;
        }
    };

    // Files that don't parse or can't run.
    let mut broken = 0;
    let mut files = Vec::new();
    for dir in [home.tasks(), home.schedules()] {
        match md_files(&dir) {
            Ok(found) => files.extend(found),
            Err(e) => add(Check::Warn(format!("{e:#}"))),
        }
    }
    let mut agents_used = std::collections::BTreeSet::new();
    let mut wants_pr = false;
    let mut stale_running = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let parsed: Result<(Vec<String>, _)> = if frontmatter::has_key(&text, "cron") {
            Schedule::parse(&text).map(|s| (s.problems(&config, Some(path)), s.spec))
        } else {
            Task::parse(&text).map(|t| {
                if t.status == Status::Running {
                    stale_running.push(t.clone());
                }
                (t.problems(&config, Some(path)), t.spec)
            })
        };
        match parsed {
            Ok((problems, spec)) => {
                let eff = spec.resolve(&config.defaults);
                agents_used.extend(eff.agent);
                wants_pr |= eff.on_finish == crate::spec::OnFinish::Pr;
                if !problems.is_empty() {
                    broken += 1;
                    add(Check::Warn(format!(
                        "{}: {}",
                        path.display(),
                        problems.join("; ")
                    )));
                }
            }
            Err(e) => {
                broken += 1;
                add(Check::Warn(format!("{}: {e:#}", path.display())));
            }
        }
    }
    if broken == 0 {
        add(Check::Ok(format!(
            "{} task/schedule file(s) are valid",
            files.len()
        )));
    }

    if config.defaults.agent.is_none() {
        add(Check::Warn(
            "no default agent is set: add `agent = \"claude\"` (or another) under [defaults] \
             in config.toml, or tasks must name one"
                .into(),
        ));
    }
    agents_used.extend(config.defaults.agent.clone());
    for name in &agents_used {
        let program = config.agents.get(name).and_then(|a| a.headless.first());
        match program {
            Some(p) if on_path(p) || std::path::Path::new(p).is_file() => {
                add(Check::Ok(format!("agent `{name}`: `{p}` is installed")));
            }
            Some(p) => add(Check::Warn(format!(
                "agent `{name}`: `{p}` is not on $PATH"
            ))),
            None => {}
        }
    }
    if wants_pr {
        if on_path("gh") {
            add(Check::Ok("gh is installed (for on_finish: pr)".into()));
        } else {
            add(Check::Warn(
                "gh is not on $PATH, but some task or schedule uses on_finish: pr".into(),
            ));
        }
    }

    if !stale_running.is_empty() {
        let session = &config.runner.tmux_session;
        let panes = crate::tmux::panes(session).unwrap_or_default();
        for t in stale_running {
            if !panes
                .iter()
                .any(|p| p.window == run::window_of(&t, &config).1)
            {
                add(Check::Warn(format!(
                    "{} is running but has no tmux window; `furmi run` will requeue it",
                    t.id
                )));
            }
        }
    }

    add(web_check(&config));

    checks
}

/// Where the dashboard is reachable, from `[web]` in config.toml: the URL
/// when it serves other machines, and what to do about it when it doesn't.
fn web_check(config: &Config) -> Check {
    let web = &config.web;
    let listen = web
        .listen
        .clone()
        .unwrap_or_else(|| "127.0.0.1:7433".to_string());
    if !crate::web::is_loopback_authority(&listen) {
        let any_host = web.allowed_hosts.iter().any(|h| h.trim() == "*");
        let Some(host) = web.allowed_hosts.first() else {
            return Check::Warn(format!(
                "the dashboard listens on {listen} but [web] allowed_hosts is empty, so every \
                 request would be rejected: run `furmi web --setup`"
            ));
        };
        // `*` has no name to advertise: use the box's tailnet name when that's
        // visible, and just name the address otherwise.
        let url = if any_host {
            crate::web::tailnet()
                .map(|tailnet| {
                    format!(
                        "{}, under any Host name",
                        crate::web::url_for(&tailnet.name, &listen)
                    )
                })
                .unwrap_or_else(|| format!("any Host name on {listen}"))
        } else {
            crate::web::url_for(host, &listen)
        };
        let Some(path) = &web.token_file else {
            return Check::Warn(format!(
                "the dashboard listens on {listen} but [web] has no token_file, so `furmi web` \
                 won't start: run `furmi web --setup`"
            ));
        };
        let path = crate::home::expand_tilde(path);
        return if path.exists() {
            Check::Ok(format!(
                "dashboard: {url} (every request needs the token in {})",
                crate::home::contract_tilde(&path)
            ))
        } else {
            Check::Warn(format!(
                "the dashboard's token file {} is missing: `furmi web --setup` creates it",
                crate::home::contract_tilde(&path)
            ))
        };
    }
    match crate::web::tailnet() {
        Some(tailnet) => Check::Warn(format!(
            "the dashboard is reachable from this machine only (http://127.0.0.1:7433). To open it \
             from your tailnet, run `furmi web --setup` once, then \
             {} (every request carries the token it writes)",
            crate::web::url_for(&tailnet.name, "0.0.0.0:7433")
        )),
        None => Check::Ok("dashboard: this machine only (http://127.0.0.1:7433)".into()),
    }
}

fn report(checks: Vec<Check>) -> Result<ExitCode> {
    let mut failed = false;
    for c in &checks {
        match c {
            Check::Ok(m) => println!("ok    {m}"),
            Check::Warn(m) => println!("warn  {m}"),
            Check::Fail(m) => {
                failed = true;
                println!("FAIL  {m}");
            }
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[derive(clap::ValueEnum, Clone, Copy)]
pub enum Manager {
    Systemd,
    Launchd,
}

/// Prints the unit files that keep `furmi run` and the dashboard running (and
/// restart them), for systemd (Linux) or launchd (macOS), with the
/// instructions to install them.
pub fn service(home: &Home, manager: Manager) -> Result<()> {
    print!("{}", units(home, manager)?);
    Ok(())
}

/// The text [`service`] prints, as a String so it can be checked.
fn units(home: &Home, manager: Manager) -> Result<String> {
    let exe = crate::exe::path().context("locating the furmi binary")?;
    let exe = exe.to_str().context("the furmi path is not valid UTF-8")?;
    let root = home
        .root()
        .to_str()
        .context("the home path is not valid UTF-8")?;
    let path = std::env::var("PATH").unwrap_or_default();
    if exe.contains('"') || root.contains('"') || path.contains('"') {
        bail!("paths with double quotes can't be put in a unit file");
    }
    Ok(match manager {
        Manager::Systemd => format!(
            "# furmiculi as user services: the queue runner and the dashboard.\n\
             # Save the two blocks below as ~/.config/systemd/user/furmiculi.service and\n\
             # ~/.config/systemd/user/furmiculi-web.service, then:\n\
             #   systemctl --user daemon-reload\n\
             #   systemctl --user enable --now furmiculi furmiculi-web\n\
             #   loginctl enable-linger $USER    # keep them running when logged out\n\
             #   journalctl --user -u furmiculi -u furmiculi-web -f\n\
             #\n\
             # The dashboard follows [web] in config.toml (`furmi web --setup` writes it). With\n\
             # nothing there it listens on 127.0.0.1:7433: this machine only.\n\
             \n\
             # ~/.config/systemd/user/furmiculi.service\n\
             [Unit]\n\
             Description=furmiculi runner\n\
             \n\
             [Service]\n\
             ExecStart=\"{exe}\" --home \"{root}\" run\n\
             Environment=\"PATH={path}\"\n\
             Restart=always\n\
             RestartSec=10\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n\
             \n\
             # ~/.config/systemd/user/furmiculi-web.service\n\
             [Unit]\n\
             Description=furmiculi dashboard\n\
             After=network-online.target\n\
             \n\
             [Service]\n\
             ExecStart=\"{exe}\" --home \"{root}\" web --no-open\n\
             Environment=\"PATH={path}\"\n\
             Restart=always\n\
             RestartSec=10\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        ),
        Manager::Launchd => {
            let xml = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;");
            let plist = |label: &str, extra: &[&str], log: &str| {
                let extra = extra
                    .iter()
                    .map(|a| format!("\x20   <string>{}</string>\n", xml(a)))
                    .collect::<String>();
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                     <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                     <plist version=\"1.0\">\n\
                     <dict>\n\
                     \x20 <key>Label</key><string>{label}</string>\n\
                     \x20 <key>ProgramArguments</key>\n\
                     \x20 <array>\n\
                     \x20   <string>{exe}</string>\n\
                     \x20   <string>--home</string>\n\
                     \x20   <string>{root}</string>\n\
                     {extra}\x20 </array>\n\
                     \x20 <key>EnvironmentVariables</key>\n\
                     \x20 <dict><key>PATH</key><string>{path}</string></dict>\n\
                     \x20 <key>RunAtLoad</key><true/>\n\
                     \x20 <key>KeepAlive</key><true/>\n\
                     \x20 <key>StandardOutPath</key><string>{root}/{log}</string>\n\
                     \x20 <key>StandardErrorPath</key><string>{root}/{log}</string>\n\
                     </dict>\n\
                     </plist>\n",
                    exe = xml(exe),
                    root = xml(root),
                    path = xml(&path),
                )
            };
            format!(
                "<!-- furmiculi as launchd agents: the queue runner and the dashboard.\n\
                 \x20    Save each block below as ~/Library/LaunchAgents/<its Label>.plist, then:\n\
                 \x20    launchctl load ~/Library/LaunchAgents/dev.furmiculi.plist\n\
                 \x20    launchctl load ~/Library/LaunchAgents/dev.furmiculi-web.plist\n\
                 \x20    The dashboard follows [web] in config.toml (`furmi web --setup` writes it).\n\
                 \x20    With nothing there it listens on 127.0.0.1:7433: this machine only. -->\n\
                 {}\n\
                 <!-- dev.furmiculi-web.plist -->\n\
                 {}",
                plist("dev.furmiculi", &["run"], "runner.stdout.log"),
                plist("dev.furmiculi-web", &["web", "--no-open"], "web.stdout.log"),
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 0600 token file in `dir`.
    fn token_file(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("web-token");
        std::fs::write(&path, "t\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[test]
    fn the_dashboard_check_says_where_to_reach_it() {
        let dir = tempfile::tempdir().unwrap();
        let token = token_file(dir.path());

        // Set up for the tailnet: the check is the URL to open.
        let config = Config::parse(&format!(
            "[web]\nlisten = \"0.0.0.0:7433\"\nallowed_hosts = [\"box.ts.net\"]\n\
             token_file = \"{}\"\n",
            token.display()
        ))
        .unwrap();
        match web_check(&config) {
            Check::Ok(message) => {
                assert!(message.contains("http://box.ts.net:7433"), "{message}");
                assert!(message.contains("web-token"), "{message}");
            }
            other => panic!("expected ok, got {}", other.message()),
        }

        // A missing token file, a missing allowlist, and a missing token_file
        // each say what to run.
        let missing = Config::parse(&format!(
            "[web]\nlisten = \"0.0.0.0:7433\"\nallowed_hosts = [\"box.ts.net\"]\n\
             token_file = \"{}\"\n",
            dir.path().join("gone").display()
        ))
        .unwrap();
        let message = web_check(&missing).message().to_string();
        assert!(message.contains("is missing"), "{message}");
        assert!(message.contains("--setup"), "{message}");

        // Any Host: no name to advertise, and never a literal `*` URL.
        let any = Config::parse(&format!(
            "[web]\nlisten = \"0.0.0.0:7433\"\nallowed_hosts = [\"*\"]\n\
             token_file = \"{}\"\n",
            token.display()
        ))
        .unwrap();
        let message = web_check(&any).message().to_string();
        assert!(!message.contains("http://*"), "{message}");
        assert!(message.contains("any Host"), "{message}");

        for text in [
            "[web]\nlisten = \"0.0.0.0:7433\"\ntoken_file = \"~/.furmiculi/web-token\"\n",
            "[web]\nlisten = \"0.0.0.0:7433\"\nallowed_hosts = [\"box.ts.net\"]\n",
        ] {
            let config = Config::parse(text).unwrap();
            let message = web_check(&config).message().to_string();
            assert!(message.contains("--setup"), "{text}: {message}");
        }
    }

    #[test]
    fn the_units_keep_the_runner_and_the_dashboard() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();

        let systemd = units(&home, Manager::Systemd).unwrap();
        assert!(
            systemd.contains(&format!("--home \"{}\"", dir.path().display())),
            "{systemd}"
        );
        assert!(systemd.contains(" run\n"), "{systemd}");
        assert!(systemd.contains(" web --no-open\n"), "{systemd}");
        assert_eq!(systemd.matches("[Unit]").count(), 2, "{systemd}");
        assert_eq!(systemd.matches("Restart=always").count(), 2, "{systemd}");

        let launchd = units(&home, Manager::Launchd).unwrap();
        assert!(launchd.contains("<string>run</string>"), "{launchd}");
        assert!(launchd.contains("<string>web</string>"), "{launchd}");
        assert!(launchd.contains("dev.furmiculi-web"), "{launchd}");
        assert_eq!(launchd.matches("<plist version").count(), 2, "{launchd}");
    }
}
