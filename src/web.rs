//! `furmi web`: a Sidekiq-style dashboard over the queue, with actions.
//!
//! Serves one HTML shell, a `/api/state` JSON endpoint the page polls, a
//! `/api/tasks/<id>/logs` endpoint, `/api/tasks/<id>/{done,fail,cancel,retry}`
//! POST routes that call straight into the same code `furmi done|fail|cancel|retry`
//! use, `POST /api/tasks` to queue a new task (the "New task" form),
//! `POST /api/tasks/<id>/edit` for a task that hasn't started, schedule
//! create/edit/pause/resume/delete under `/api/schedules`, `POST
//! /api/runner/{start,stop}`, and `GET /api/doctor` and `/api/agents` (plus
//! `POST /api/agents/default`) for the setup views.
//!
//! Two ways to listen, both guarded:
//!
//! - **Loopback (the default, `127.0.0.1:<port>`).** A web page you visit can
//!   still send requests there, so every request must carry a loopback `Host`
//!   (defeats DNS rebinding), and POSTs a per-run token that only the
//!   dashboard page itself can read (defeats cross-site forgery) and a
//!   loopback `Origin`, if any.
//! - **Remote (`--listen` with a non-loopback address, e.g. `0.0.0.0:7433`).**
//!   For a headless box reached from browsers on other machines (a tailnet).
//!   Every request — not just POSTs — must carry a shared bearer token
//!   (`Authorization: Bearer …`). The first page load may pass it as
//!   `?token=` instead; the server then sets an `HttpOnly` cookie for it, so
//!   a refresh or a bookmark doesn't have to carry the token in the URL (the
//!   page itself keeps it in memory only and strips it from the address
//!   bar). Every `Host` must match one exact `--allowed-host` entry (defeats
//!   DNS rebinding over the wide bind). The per-run POST token and Origin
//!   checks stay on top of the bearer gate. A non-loopback bind refuses to
//!   start without a token and an allowlist.
//!
//! Answering those three things is a chore, so it's asked once: `furmi web
//! --setup` — and the same questions on their own the first time a wide bind
//! is asked for without them — writes them into `[web]` in config.toml (see
//! [`crate::config::Web`]) and creates the token file. Later runs are plain
//! `furmi web`, with flags still winning over the saved answers.

use std::cmp::Reverse;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::cmd::{NewTask, create_task, md_files};
use crate::config::{Config, Web, on_path, set_web_config};
use crate::control;
use crate::edit::{self, ScheduleEdit, Start, TaskEdit, on_finish_str};
use crate::home::{Home, contract_tilde, expand_tilde};
use crate::run::{self, RunnerState};
use crate::schedule::Schedule;
use crate::spec::{OnFinish, TaskSpec};
use crate::task::{Status, Task, now};

#[derive(clap::Args)]
pub struct WebArgs {
    /// Port to listen on (ignored when --listen names the port itself).
    #[arg(long, default_value_t = 7433)]
    port: u16,
    /// Address to bind, e.g. `0.0.0.0:7433` to serve other machines (a
    /// tailnet). Default, and unless the address is non-loopback, loopback
    /// checks apply unchanged.
    #[arg(long)]
    listen: Option<String>,
    /// File with the bearer token for `--listen` (created/made 0600 by the
    /// operator; read once at startup, kept out of argv).
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Bearer token for `--listen`, right on the command line (--token-file
    /// is preferred: command lines end up in shell history and ps output).
    #[arg(long)]
    token: Option<String>,
    /// Host an HTTP `Host` header must carry exactly to be served, for
    /// non-loopback `--listen` binds. Repeatable. `host`, `host:port`, or `*`
    /// to accept any host (the token still gates every request).
    #[arg(long = "allowed-host")]
    allowed_hosts: Vec<String>,
    /// Ask where to serve from (your tailnet? which host name?) and save the
    /// answers in config.toml, then serve. Also runs on its own the first
    /// time a non-loopback `--listen` is given without a token and a host.
    #[arg(long)]
    setup: bool,
    /// Stop a dashboard already serving this address in this home and take
    /// it over, instead of failing. When the address is busy and furmi can
    /// tell whose dashboard it is, running in a terminal asks instead.
    #[arg(long)]
    replace: bool,
    /// Don't open the dashboard in a browser.
    #[arg(long)]
    no_open: bool,
    /// Serve in the background and return: the dashboard (and the runner, for
    /// `furmi start`) keeps going after this shell closes, logging to web.log
    /// in the data folder. `furmi stop` stops it.
    #[arg(long, short = 'd')]
    detach: bool,
}

const INDEX_HTML: &str = include_str!("web_dashboard.html");
const ACTIONS: [&str; 4] = ["done", "fail", "cancel", "retry"];
/// Requests are handled on this many threads, so one slow one (say, an
/// action waiting for the lock) doesn't stall the dashboard's polling.
const WORKERS: usize = 4;
/// Largest request body read; the only body anyone sends is a short reason.
const MAX_BODY_BYTES: u64 = 16 * 1024;
const TOKEN_HEADER: &str = "X-Furmi-Token";
/// The cookie a remote bind hands the browser after a first load carrying
/// `?token=`, so a refresh (or a bookmark) doesn't ask for the token again.
const TOKEN_COOKIE: &str = "furmi_token";
/// How long that cookie lives. Long enough that a bookmark still works next
/// month; it is re-set on every page load.
const TOKEN_COOKIE_MAX_AGE: u64 = 30 * 24 * 60 * 60;

struct Ctx<'a> {
    home: &'a Home,
    token: String,
    index: String,
    /// The git repo `furmi web` was started in, offered first in the new-task form.
    cwd_repo: Option<String>,
    /// A non-loopback bind: requests additionally need the bearer token and
    /// an allowlisted `Host`.
    remote: bool,
    /// Exact `Host` allowlist for a remote bind.
    allowed_hosts: Vec<String>,
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
    /// A binary body (the logo), served as it is. `body` stays empty then.
    asset: Option<&'static [u8]>,
}

fn reply(status: u16, content_type: &'static str, body: impl Into<String>) -> Reply {
    Reply {
        status,
        content_type,
        body: body.into(),
        asset: None,
    }
}

/// The ant from the README, cropped to the mark itself and carried inside the
/// binary: no file to install, no request off the box, and the same picture the
/// repository shows. Two of them, because the artwork's own colours are two:
/// dark brown for a light page, cream for a dark one.
const LOGO: &[u8] = include_bytes!("../assets/logo-mark.png");
const LOGO_DARK: &[u8] = include_bytes!("../assets/logo-mark-dark.png");

fn json_reply(status: u16, body: String) -> Reply {
    reply(status, "application/json", body)
}

pub fn serve(home: &Home, args: WebArgs) -> Result<()> {
    home.ensure_initialized()?;
    let serving = setup(home, &args)?;
    if args.detach {
        return detach(home, false, args, &serving);
    }
    let server = bind(home, &serving, args.replace)?;
    run_server(home, server, &serving, &args)
}

/// `furmi start`: the runner and the dashboard in one command. The runner
/// stops when the dashboard does, so there's nothing to clean up.
pub fn start(home: &Home, args: WebArgs) -> Result<()> {
    home.ensure_initialized()?;
    let serving = setup(home, &args)?;
    if args.detach {
        return detach(home, true, args, &serving);
    }
    let server = bind(home, &serving, args.replace)?;
    let _runner = if home.runner_alive() {
        println!("The runner is already running.");
        None
    } else {
        Some(spawn_runner(home)?)
    };
    run_server(home, server, &serving, &args)
}

/// How long a backgrounded dashboard gets to record itself before `furmi
/// start -d` calls the spawn a failure.
const DETACH_WAIT: Duration = Duration::from_secs(10);

/// Serves the dashboard (`with_runner`, for `furmi start`: the runner too) in
/// its own process group with its output in `web.log`, then returns — so the
/// shell that started it gets its prompt back and closing it changes nothing.
/// Everything worth reading is printed here, since the child has no terminal.
fn detach(home: &Home, with_runner: bool, args: WebArgs, serving: &Serving) -> Result<()> {
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

/// How the dashboard listens. `remote` is a wide, headless-style bind:
/// requests then have to prove themselves with a bearer token and an
/// allowlisted `Host`, on top of the loopback-era checks.
#[derive(Debug)]
struct Serving {
    addr: String,
    remote: bool,
    /// Shared secret for a remote bind; None on the loopback default, where
    /// the per-run token in the page is the only secret.
    bearer: Option<String>,
    /// Exact `Host` allowlist for a remote bind.
    allowed_hosts: Vec<String>,
    /// Where the bearer came from (`~/.furmiculi/web-token`, `--token`), for
    /// the line that says how to get it.
    token_from: Option<String>,
}

impl Serving {
    /// The URL a browser on another machine opens, for the first allowlisted
    /// host. `with_token` puts the secret in the query string: only do that
    /// on a terminal, never into a log file or the journal.
    fn open_url(&self, token: &str, with_token: bool) -> Option<String> {
        let url = match self.allowed_hosts.first()? {
            // Any Host is accepted, so there's no one name to print: say where
            // to put the box's name (or IP) instead.
            any if is_any_host(any) => {
                let port = split_authority(&self.addr).1.unwrap_or("7433");
                format!("http://<this box's tailnet name or IP>:{port}")
            }
            host => url_for(host, &self.addr),
        };
        Some(if with_token {
            format!("{url}/?token={token}")
        } else {
            format!("{url}/?token=<the token>")
        })
    }
}

/// The three things a wide bind needs, from the flags, then `[web]` in
/// config.toml, then the defaults. Asks (once, saving the answers) when
/// they're not there: `--setup` always asks, and so does whatever `furmi web`
/// runs the first time it's pointed at a non-loopback address.
fn setup(home: &Home, args: &WebArgs) -> Result<Serving> {
    let mut web = match Config::load(&home.config_path()) {
        Ok(config) => config.web,
        // The dashboard is where a broken config gets explained (see
        // `state_json`), so it doesn't refuse to start over one: it serves
        // loopback, the safe default, and says what's wrong.
        Err(e) => {
            eprintln!("warning: {e:#} (serving the dashboard on loopback)");
            Web::default()
        }
    };
    // Asked once, ever, on a terminal: either because nobody has answered yet
    // (a first run) or because a wide address is missing its token or hosts.
    let asks = args.setup
        || (std::io::stdin().is_terminal() && (never_asked(&web) || unanswered(args, &web)));
    if asks {
        web = interactive_setup(home, args, &web)?;
    }
    resolve(args, &web)
}

/// Has nobody answered `furmi web --setup`'s questions for this home yet?
/// A table with any serving setting counts as answered, so a hand-written
/// `[web]` is never asked about.
fn never_asked(web: &Web) -> bool {
    !web.configured
        && web.listen.is_none()
        && web.allowed_hosts.is_empty()
        && web.token_file.is_none()
}

/// Would a wide bind be missing its token or its allowed host — the questions
/// `--setup` asks?
fn unanswered(args: &WebArgs, web: &Web) -> bool {
    let addr = addr_of(args, web);
    if is_loopback_authority(host_of(&addr)) {
        return false;
    }
    let has_token = args.token.is_some() || args.token_file.is_some() || web.token_file.is_some();
    let has_hosts = !args.allowed_hosts.is_empty() || !web.allowed_hosts.is_empty();
    !(has_token && has_hosts)
}

fn resolve(args: &WebArgs, web: &Web) -> Result<Serving> {
    let addr = addr_of(args, web);
    let remote = !is_loopback_authority(host_of(&addr));
    let (bearer, token_from) = token_of(args, web)?;
    if !remote {
        return Ok(Serving {
            addr,
            remote: false,
            bearer: None,
            allowed_hosts: Vec::new(),
            token_from: None,
        });
    }
    let Some(bearer) = bearer else {
        anyhow::bail!(
            "{addr} is not a loopback bind: browsers on other machines could reach it, \
             so it needs a shared secret. Run `furmi web --setup` to answer that once (the \
             answer is saved in config.toml), or pass --token-file <file> (preferred) or \
             --token <value>"
        );
    };
    let allowed_hosts = if args.allowed_hosts.is_empty() {
        web.allowed_hosts.clone()
    } else {
        args.allowed_hosts.clone()
    };
    if allowed_hosts.is_empty() {
        anyhow::bail!(
            "{addr} is not a loopback bind: without an allowed host no Host header could \
             ever be trusted, so every request would be rejected. Run `furmi web --setup`, or \
             pass --allowed-host <name> — the name browsers use, e.g. this box's tailnet name"
        );
    }
    Ok(Serving {
        addr,
        remote: true,
        bearer: Some(bearer),
        allowed_hosts,
        token_from,
    })
}

/// `--listen`, else `[web] listen`, else loopback on `--port`. A bind address
/// with no port takes `--port`.
fn addr_of(args: &WebArgs, web: &Web) -> String {
    let addr = args
        .listen
        .clone()
        .or_else(|| web.listen.clone())
        .unwrap_or_else(|| format!("127.0.0.1:{}", args.port));
    if host_of(&addr) == addr {
        format!("{addr}:{}", args.port)
    } else {
        addr
    }
}

/// The shared secret for a wide bind, and where it came from.
fn token_of(args: &WebArgs, web: &Web) -> Result<(Option<String>, Option<String>)> {
    match (&args.token_file, &args.token) {
        (Some(_), Some(_)) => {
            anyhow::bail!("give the token one way: --token-file or --token, not both")
        }
        (Some(path), None) => Ok((
            Some(read_token_file(path)?),
            Some(contract_tilde(&expand_tilde(path))),
        )),
        (None, Some(token)) => Ok((Some(token.clone()), Some("--token".to_string()))),
        (None, None) => match &web.token_file {
            Some(path) => {
                let path = expand_tilde(path);
                Ok((Some(read_token_file(&path)?), Some(contract_tilde(&path))))
            }
            None => Ok((None, None)),
        },
    }
}

/// The questions a wide bind needs answered, asked once: where it listens,
/// which Host names browsers use, and (silently) a token file only this user
/// can read. Saved in config.toml, so the next `furmi web` asks nothing.
fn interactive_setup(home: &Home, args: &WebArgs, web: &Web) -> Result<Web> {
    println!("Setting up the dashboard; this is saved, so it only asks once.");
    let listen = args
        .listen
        .clone()
        .or_else(|| web.listen.clone())
        .unwrap_or_else(|| format!("0.0.0.0:{}", args.port));

    if !ask_yes_no(
        "Serve other machines (your tailnet), not just this one?",
        true,
    )? {
        // Saved like any other answer, so this doesn't come up again;
        // `furmi web --setup` asks again whenever you change your mind.
        let web = Web {
            configured: true,
            ..Web::default()
        };
        set_web_config(&home.config_path(), &web)?;
        println!(
            "Okay: `furmi web` serves http://127.0.0.1:{} on this machine only \
             (saved in {}; `furmi web --setup` asks again).",
            args.port,
            contract_tilde(&home.config_path())
        );
        return Ok(web);
    }

    // Saved answers first, then what tailscale says this box is called, so
    // running --setup again is a matter of pressing enter.
    let (suggested, why_not) = if web.allowed_hosts.is_empty() {
        match tailnet() {
            TailnetLookup::Found(tailnet) => (tailnet.hosts().join(", "), None),
            TailnetLookup::Unavailable(reason) => (String::new(), Some(reason)),
        }
    } else {
        (web.allowed_hosts.join(", "), None)
    };
    // Nothing to suggest (a container whose tailscaled runs on the host, say):
    // say so instead of asking into the void, and let enter mean "any Host" —
    // the token still gates every request.
    let default = if !suggested.is_empty() {
        suggested.clone()
    } else {
        println!(
            "  No tailnet name to suggest ({}) — on the machine that runs tailscale it's in\n  \
             `tailscale status --json` (the MagicDNS name), and `furmi web --setup` takes it later.",
            why_not.unwrap_or_else(|| "tailscale isn't installed".into())
        );
        ANY_HOST.to_string()
    };
    let hosts = loop {
        let answer = ask(
            "Host name(s) browsers use to reach this box, comma-separated (`*` accepts any)",
            Some(default.as_str()),
        )?;
        let hosts = parse_hosts(if answer.is_empty() {
            &suggested
        } else {
            &answer
        });
        if hosts.is_empty() {
            println!(
                "  need at least one, e.g. this-box.ts.net or 100.64.0.10 — or `*` to accept \
                 any Host"
            );
            continue;
        }
        break hosts;
    };

    let token_file = home.root().join("web-token");
    ensure_token_file(&token_file)?;
    let web = Web {
        configured: true,
        listen: Some(listen.clone()),
        allowed_hosts: hosts,
        token_file: Some(PathBuf::from(contract_tilde(&token_file))),
    };
    set_web_config(&home.config_path(), &web)?;
    println!(
        "Saved to {}: listening on {listen}; Host: {}; token in {}.",
        contract_tilde(&home.config_path()),
        hosts_summary(&web.allowed_hosts),
        contract_tilde(&token_file)
    );
    Ok(web)
}

/// This box as its tailnet sees it: the MagicDNS name and the tailscale IPv4
/// a browser might use instead. None without a working, logged-in tailscale.
pub struct Tailnet {
    pub name: String,
    pub ip: Option<String>,
}

impl Tailnet {
    /// Names the dashboard should accept, advertised in this order.
    pub fn hosts(&self) -> Vec<String> {
        std::iter::once(self.name.clone())
            .chain(self.ip.clone())
            .collect()
    }
}

/// [`tailnet`], and why there's nothing to offer when there isn't.
pub enum TailnetLookup {
    Found(Tailnet),
    /// No name: `tailscale` isn't installed, or its daemon isn't reachable
    /// from here — on a container the daemon usually runs on the host, so
    /// this box can't ask it what it's called.
    Unavailable(String),
}

impl TailnetLookup {
    pub fn found(self) -> Option<Tailnet> {
        match self {
            Self::Found(tailnet) => Some(tailnet),
            Self::Unavailable(_) => None,
        }
    }
}

pub fn tailnet() -> TailnetLookup {
    let mut cmd = Command::new("tailscale");
    cmd.args(["status", "--json"]);
    let out = match crate::fsutil::run_with_timeout(cmd, Duration::from_secs(5)) {
        Ok(out) => out,
        Err(e) => return TailnetLookup::Unavailable(format!("`tailscale status` didn't run: {e}")),
    };
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let said = said.lines().next().unwrap_or("").trim();
        // Drop tailscale's own hint, like "(sudo systemctl start tailscaled ?)":
        // in a container it points at the wrong machine.
        let said = said.split(" (").next().unwrap_or(said).trim();
        return TailnetLookup::Unavailable(if said.is_empty() {
            "`tailscale status` failed".to_string()
        } else {
            format!("tailscale says: {said}")
        });
    }
    parse_tailnet_status(&out.stdout)
}

/// The MagicDNS name and IPv4 out of `tailscale status --json`, split out so
/// the shapes it comes in are testable without a tailnet.
fn parse_tailnet_status(json: &[u8]) -> TailnetLookup {
    let Ok(status) = serde_json::from_slice::<serde_json::Value>(json) else {
        return TailnetLookup::Unavailable("`tailscale status` printed nothing usable".into());
    };
    let Some(name) = status
        .pointer("/Self/DNSName")
        .and_then(|name| name.as_str())
        .map(|name| name.trim_end_matches('.').to_string())
        .filter(|name| !name.is_empty())
    else {
        return TailnetLookup::Unavailable("tailscale isn't logged in on this machine".into());
    };
    let ip = status
        .pointer("/Self/TailscaleIPs/0")
        .and_then(|ip| ip.as_str())
        .map(str::to_string);
    TailnetLookup::Found(Tailnet { name, ip })
}

/// Creates the token file (0600, 128 random bits) if it isn't there yet. An
/// existing one is left alone, so cookies already handed out keep working.
fn ensure_token_file(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if path.exists() {
        return Ok(());
    }
    let token = random_token()?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", contract_tilde(path)))?;
    writeln!(file, "{token}")
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", contract_tilde(path)))
}

/// One answer from stdin. `default` is shown in brackets and used for an
/// empty answer; a closed stdin is an error, not a default — a setup that
/// half-ran from a script shouldn't quietly serve a wide bind.
fn ask(prompt: &str, default: Option<&str>) -> Result<String> {
    let hint = default.map(|d| format!(" [{d}]")).unwrap_or_default();
    print!("{prompt}{hint}: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        println!();
        anyhow::bail!(
            "no answer (stdin closed): run `furmi web --setup` in a terminal, or pass \
             --listen, --allowed-host and --token-file by hand"
        );
    }
    let answer = line.trim();
    Ok(if answer.is_empty() {
        default.unwrap_or_default().to_string()
    } else {
        answer.to_string()
    })
}

fn ask_yes_no(prompt: &str, default: bool) -> Result<bool> {
    let answer = ask(
        &format!("{prompt} (y/n)"),
        Some(if default { "y" } else { "n" }),
    )?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        other => anyhow::bail!("expected y or n, got `{other}`"),
    }
}

/// `"box.ts.net, 100.64.0.5"` as allowlist entries: split on commas and
/// whitespace, trailing dots off (MagicDNS names carry one), duplicates
/// dropped.
fn parse_hosts(text: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for host in text.split([',', ' ', '\t']) {
        let host = host.trim().trim_end_matches('.');
        if !host.is_empty() && !hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            hosts.push(host.to_string());
        }
    }
    hosts
}

/// How the allowlist reads in a message: the names, or "any Host" for `*`.
fn hosts_summary(hosts: &[String]) -> String {
    if hosts.iter().any(|h| is_any_host(h)) {
        "any Host".to_string()
    } else {
        hosts.join(", ")
    }
}

/// A URL for an allowlist entry: its own port if it names one, else the port
/// the dashboard listens on (80 stays implicit).
pub(crate) fn url_for(entry: &str, listen: &str) -> String {
    let (host, entry_port) = split_authority(entry);
    let listen_port = split_authority(listen).1.unwrap_or("7433");
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match entry_port {
        Some(port) => format!("http://{host}:{port}"),
        None if listen_port == "80" => format!("http://{host}"),
        None => format!("http://{host}:{listen_port}"),
    }
}

/// Reads the token file: one line, trimmed. It should be 0600; a group- or
/// world-readable one is refused, the way a shared secret shouldn't sit.
fn read_token_file(path: &std::path::Path) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let path = &expand_tilde(path);
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", contract_tilde(path)))?;
    let mode = std::fs::metadata(path)
        .with_context(|| format!("checking {}", contract_tilde(path)))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        anyhow::bail!(
            "{} must be readable only by you (chmod 600): a token the whole group \
             or everyone can read isn't a secret",
            contract_tilde(path)
        );
    }
    let token = raw.trim();
    if token.is_empty() {
        anyhow::bail!(
            "{} is empty: put a token in it (any long random string)",
            contract_tilde(path)
        );
    }
    Ok(token.to_string())
}

/// `127.0.0.1:7433`, `0.0.0.0:7433`, `[::1]:7433` — the host part alone.
fn host_of(addr: &str) -> &str {
    match addr.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => match addr.rsplit_once(':') {
            // `127.0.0.1:7433`, but not bare IPv6 (`::1` is handled exactly
            // by the match in `is_loopback_authority`).
            Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                host
            }
            _ => addr,
        },
    }
}

/// Binds the dashboard. When something already holds the address, and it's
/// this home's own dashboard, offer to stop it and take over (`--replace`, or
/// a yes at the prompt); everything else keeps the original error.
fn bind(home: &Home, serving: &Serving, replace: bool) -> Result<Server> {
    let addr = &serving.addr;
    let busy = match Server::http(addr.as_str()) {
        Ok(server) => return Ok(server),
        // `&*e`: the box derefs to the error it holds; a `&Box<dyn Error>`
        // wouldn't coerce, since `Box<dyn Error>` isn't itself `Error`.
        Err(e) if !address_in_use(&*e) => return Err(busy_error(addr, &e.to_string())),
        Err(e) => e.to_string(),
    };
    // The port is taken. Only one dashboard per home is recorded, and only
    // that one is a candidate: for anything else, `kill` is your business.
    let Some((pid, served)) = home
        .dashboard()
        .filter(|(_, served)| same_port(served, addr))
    else {
        return Err(busy_error(addr, &busy));
    };
    let take_over = replace
        || (std::io::stdin().is_terminal()
            && ask_yes_no(
                &format!(
                    "A dashboard is already serving http://{served} (pid {pid}). Stop it and \
                     serve this address instead?"
                ),
                false,
            )?);
    if !take_over {
        anyhow::bail!(
            "a dashboard is already serving http://{served} (pid {pid}): open that, or take the \
             address over with `furmi web --replace`"
        );
    }
    stop_dashboard(pid, addr)?.map_or_else(
        || {
            Err(busy_error(
                addr,
                &format!("the dashboard on it (pid {pid}) stopped, but the address is still busy"),
            ))
        },
        Ok,
    )
}

/// Whether the bind failed because something already holds the port.
fn address_in_use(e: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    match e.downcast_ref::<std::io::Error>() {
        Some(io) => io.kind() == std::io::ErrorKind::AddrInUse,
        // tiny_http hands back its bind error boxed; if it ever stops exposing
        // the io::Error, the message still says what happened.
        None => e.to_string().contains("in use"),
    }
}

fn busy_error(addr: &str, reason: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "couldn't listen on {addr}: {reason}. If `furmi web` or `furmi start` is already running, \
         open http://{addr}; otherwise pick another address with --listen"
    )
}

/// Same port, whatever the hosts are: `0.0.0.0:7433` and `127.0.0.1:7433`
/// can't both be served, so they collide in practice.
fn same_port(a: &str, b: &str) -> bool {
    let port = |addr: &str| split_authority(addr).1.and_then(|p| p.parse::<u16>().ok());
    port(a).zip(port(b)).is_some_and(|(a, b)| a == b)
}

/// Binds, retrying while the address is still busy — for the moment between
/// asking a dashboard to stop and its port actually coming free.
fn listen_waiting(addr: &str, timeout: Duration) -> Option<Server> {
    let deadline = Instant::now() + timeout;
    loop {
        match Server::http(addr) {
            Ok(server) => return Some(server),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

/// Asks a running dashboard to stop (SIGTERM, so it shuts down the way Ctrl-C
/// does) and waits for the address to come free — which is what this process
/// needs, and which takes a moment for `furmi start` (it stops its runner
/// too). A dashboard still there after ten seconds is killed.
fn stop_dashboard(pid: u32, addr: &str) -> Result<Option<Server>> {
    let Ok(pid) = i32::try_from(pid) else {
        anyhow::bail!("pid {pid} is not a valid pid")
    };
    // SAFETY: plain kill(2), on the pid this home's lock file records.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        anyhow::bail!(
            "couldn't stop the dashboard (pid {pid}): {} — stop it by hand",
            std::io::Error::last_os_error()
        );
    }
    println!("Stopping the dashboard that was serving here (pid {pid}).");
    if let Some(server) = listen_waiting(addr, Duration::from_secs(10)) {
        return Ok(Some(server));
    }
    // SAFETY: same pid; SIGKILL for one that ignored the polite request.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    Ok(listen_waiting(addr, Duration::from_secs(5)))
}

fn run_server(home: &Home, server: Server, serving: &Serving, args: &WebArgs) -> Result<()> {
    // Record this dashboard (pid and address) for as long as it serves, so a
    // second `furmi web` on the same address can offer to take over instead of
    // just failing. Best-effort: a dashboard deliberately on another address
    // doesn't need the record, and must not fail over it.
    let _record = home.claim_dashboard(&serving.addr).ok();
    // Ctrl-C / SIGTERM request a clean stop: the signal handler only sets a
    // flag, so the request loop is unblocked below instead of dying in the
    // middle of a request.
    run::install_shutdown_handler();
    let server = Arc::new(server);
    // On a remote bind the shared secret is the token: the page holds it in
    // memory (from `?token=` on first load) and sends it as both the bearer
    // and the POST token. On loopback it stays the per-run random token.
    let token = match &serving.bearer {
        Some(b) => b.clone(),
        None => random_token()?,
    };
    let cwd_repo = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::git::repo_root(&cwd).ok())
        .map(|root| contract_tilde(&root));
    let ctx = Ctx {
        home,
        index: INDEX_HTML.replace("__FURMI_TOKEN__", &token),
        token,
        cwd_repo,
        remote: serving.remote,
        allowed_hosts: serving.allowed_hosts.clone(),
    };
    let addr = &serving.addr;
    let url = format!("http://{addr}");
    let tty = std::io::stdout().is_terminal();
    println!("Dashboard: {url}  (Ctrl-C to stop)");
    if serving.remote {
        print_remote(serving, &ctx.token, tty);
    }
    // A wide bind has no local browser worth opening; loopback does.
    if !args.no_open && !serving.remote && tty {
        open_browser(&url);
    }

    // Unblock every worker once the handler has seen a signal (`unblock`
    // wakes one blocked thread per call).
    let unblocker = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            while !run::shutdown_requested() {
                std::thread::sleep(Duration::from_millis(100));
            }
            for _ in 0..WORKERS {
                server.unblock();
            }
        })
    };
    let ctx = &ctx;

    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            let server = Arc::clone(&server);
            scope.spawn(move || {
                for request in server.incoming_requests() {
                    handle(ctx, request);
                }
            });
        }
    });
    let _ = unblocker.join();
    println!("Stopping the dashboard (running tasks keep going in their tmux windows).");
    Ok(())
}

/// Where a browser on another machine reaches a wide bind, and with what: the
/// token goes in the URL only on a terminal, so a unit's journal or a log file
/// never keeps the secret around.
fn print_remote(serving: &Serving, token: &str, tty: bool) {
    if let Some(open) = serving.open_url(token, tty) {
        println!("Serving other machines. Open this once, and the page keeps a cookie:");
        println!("  {open}");
    }
    println!(
        "  Host: {}; token in {}",
        hosts_summary(&serving.allowed_hosts),
        serving.token_from.as_deref().unwrap_or("--token")
    );
}

/// A background `furmi run`, stopped when dropped. It's asked to stop
/// (SIGTERM) so it can finish its current pass first; only a runner that
/// doesn't go is killed.
struct RunnerChild(Child);

impl Drop for RunnerChild {
    fn drop(&mut self) {
        // SAFETY: plain kill(2) on the child we spawned.
        let asked = i32::try_from(self.0.id())
            .map(|pid| unsafe { libc::kill(pid, libc::SIGTERM) } == 0)
            .unwrap_or(false);
        if asked {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if self.0.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_runner(home: &Home) -> Result<RunnerChild> {
    let exe = crate::exe::path().context("finding the furmi binary to start the runner")?;
    let child = Command::new(exe)
        .arg("--home")
        .arg(home.root())
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the runner")?;
    println!(
        "Runner started (it logs to {})",
        contract_tilde(&home.runner_log())
    );
    Ok(RunnerChild(child))
}

/// Opens `url` in the default browser, if this machine has a way to. Never
/// an error: the URL is printed either way.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let url = url.to_string();
    std::thread::spawn(move || {
        let _ = Command::new(opener)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

fn handle(ctx: &Ctx, mut request: Request) {
    let url = request.url().to_string();
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    let host = header("Host");
    let origin = header("Origin");
    let token = header(TOKEN_HEADER);
    let authorization = header("Authorization");
    let cookie = header("Cookie");
    let is_post = *request.method() == Method::Post;

    let result = if !host.as_deref().is_some_and(|h| hosts_ok(ctx, h)) {
        if ctx.remote {
            reply(403, "text/plain", "forbidden: not an allowed host")
        } else {
            reply(403, "text/plain", "forbidden: not a loopback host")
        }
    } else if !bearer_ok(
        ctx,
        request.method(),
        &url,
        authorization.as_deref(),
        cookie.as_deref(),
    ) {
        reply(
            401,
            "text/plain",
            "unauthorized: missing or wrong bearer token",
        )
    } else if is_post
        && !(origin.as_deref().is_none_or(|o| origins_ok(ctx, o))
            && token.as_deref().is_some_and(|t| same_token(t, &ctx.token)))
    {
        reply(403, "text/plain", "forbidden: missing or wrong token")
    } else {
        let mut body = String::new();
        if is_post {
            let _ = request
                .as_reader()
                .take(MAX_BODY_BYTES)
                .read_to_string(&mut body);
        }
        route(ctx, request.method(), &url, &body)
    };
    let is_index = result.content_type.starts_with("text/html");
    let mut response = match result.asset {
        Some(bytes) => Response::from_data(bytes).with_status_code(result.status),
        None => Response::from_string(result.body).with_status_code(result.status),
    };
    let mut add = |name: &str, value: &str| {
        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(h);
        }
    };
    add("Content-Type", result.content_type);
    // The state behind the page is never cached; the logo is the one thing that
    // can be, and it only changes when the binary does.
    add(
        "Cache-Control",
        if result.asset.is_some() {
            "max-age=86400"
        } else {
            "no-store"
        },
    );
    add("X-Content-Type-Options", "nosniff");
    if is_index {
        add("X-Frame-Options", "DENY");
        add(
            "Content-Security-Policy",
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             img-src 'self'; connect-src 'self'; frame-ancestors 'none'",
        );
        // Remote bind: the page has the token in memory from `?token=` and
        // strips it from the address bar, so hand the browser a cookie of its
        // own — otherwise every reload would be a 401. HttpOnly (the page
        // never needs it) and Strict (a link from another site doesn't carry
        // it; a bookmark or a reload does).
        if ctx.remote {
            add(
                "Set-Cookie",
                &format!(
                    "{TOKEN_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={TOKEN_COOKIE_MAX_AGE}",
                    ctx.token
                ),
            );
        }
    }
    let _ = request.respond(response);
}

fn route(ctx: &Ctx, method: &Method, url: &str, body: &str) -> Reply {
    let home = ctx.home;
    let path = url.split('?').next().unwrap_or("");
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match method {
        Method::Get => match segments.as_slice() {
            [] => reply(200, "text/html; charset=utf-8", ctx.index.clone()),
            ["logo.png"] => Reply {
                asset: Some(LOGO),
                ..reply(200, "image/png", "")
            },
            ["logo-dark.png"] => Reply {
                asset: Some(LOGO_DARK),
                ..reply(200, "image/png", "")
            },
            ["api", "state"] => match state_json(home, ctx.cwd_repo.as_deref()) {
                Ok(json) => json_reply(200, json),
                Err(e) => json_reply(500, json_err(&e)),
            },
            ["api", "doctor"] => json_reply(200, doctor_json(home)),
            ["api", "agents"] => match agents_json(home) {
                Ok(json) => json_reply(200, json),
                Err(e) => json_reply(500, json_err(&e)),
            },
            ["api", "tasks", id] => detail_reply(edit::task_detail(home, id)),
            ["api", "tasks", id, "view"] => match task_view_json(home, id) {
                Ok(json) => json_reply(200, json),
                Err(e) => json_reply(404, json_err(&e)),
            },
            ["api", "schedules", id] => detail_reply(edit::schedule_detail(home, id)),
            ["api", "tasks", id, "logs"] => {
                let attempt = query_param(url, "attempt").and_then(|v| v.parse().ok());
                match control::logs(home, id, attempt) {
                    Ok(log) => {
                        // `?tail=<bytes>`: the end of a long log, for the live
                        // view, instead of re-reading all of it every poll.
                        let (log, cut) = match query_param(url, "tail").and_then(|v| v.parse().ok())
                        {
                            Some(limit) => tail_of(&log, limit),
                            None => (log, false),
                        };
                        json_reply(200, json_log_with(&log, cut))
                    }
                    Err(e) => json_reply(404, json_err(&e)),
                }
            }
            _ => reply(404, "text/plain", "not found"),
        },
        Method::Post => match segments.as_slice() {
            ["api", "runner", "start"] => match control::start_runner(home) {
                Ok(started) => json_reply(
                    200,
                    serde_json::json!({ "ok": true, "started": started }).to_string(),
                ),
                Err(e) => json_reply(400, json_err(&e)),
            },
            ["api", "runner", "stop"] => match control::stop_runner(home) {
                Ok(pid) => json_reply(
                    200,
                    serde_json::json!({ "ok": true, "pid": pid }).to_string(),
                ),
                Err(e) => json_reply(400, json_err(&e)),
            },
            ["api", "tasks", id, "edit"] => mutation_reply(perform_edit_task(home, id, body)),
            ["api", "schedules"] => match perform_create_schedule(home, body) {
                Ok(id) => json_reply(200, serde_json::json!({ "ok": true, "id": id }).to_string()),
                Err(e) => json_reply(400, json_err(&e)),
            },
            ["api", "schedules", id, "edit"] => {
                mutation_reply(perform_edit_schedule(home, id, body))
            }
            ["api", "schedules", id, "pause"] => {
                mutation_reply(edit::set_schedule_enabled(home, id, false))
            }
            ["api", "schedules", id, "resume"] => {
                mutation_reply(edit::set_schedule_enabled(home, id, true))
            }
            ["api", "schedules", id, "delete"] => mutation_reply(edit::delete_schedule(home, id)),
            ["api", "agents", "default"] => mutation_reply(perform_set_default_agent(home, body)),
            ["api", "tasks"] => match perform_create(home, body) {
                Ok(id) => json_reply(200, serde_json::json!({ "ok": true, "id": id }).to_string()),
                Err(e) => json_reply(400, json_err(&e)),
            },
            ["api", "tasks", id, action] if ACTIONS.contains(action) => {
                match perform_action(home, id, action, body) {
                    Ok(()) => json_reply(200, "{\"ok\":true}".into()),
                    Err(e) => json_reply(400, json_err(&e)),
                }
            }
            _ => reply(404, "text/plain", "not found"),
        },
        _ => reply(404, "text/plain", "not found"),
    }
}

/// `host`, `host:port` or `[::1]:port`: is the host part a loopback name?
pub(crate) fn is_loopback_authority(authority: &str) -> bool {
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// `http://127.0.0.1:7433` and the like. (`null`, sent by sandboxed or
/// cross-site contexts, is not.)
fn is_loopback_origin(origin: &str) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(is_loopback_authority)
}

/// Which `Host` headers this server accepts: loopback names for a loopback
/// bind, exact allowlist entries for a remote one.
fn hosts_ok(ctx: &Ctx, host: &str) -> bool {
    if ctx.remote {
        host_allowed(host, &ctx.allowed_hosts)
    } else {
        is_loopback_authority(host)
    }
}

/// Which `Origin` headers a POST accepts: loopback for either mode; a remote
/// bind additionally accepts an origin whose host is on its allowlist (the
/// dashboard page itself, loaded from `http://tools.example:7433`).
fn origins_ok(ctx: &Ctx, origin: &str) -> bool {
    is_loopback_origin(origin) || (ctx.remote && origin_allowed(origin, &ctx.allowed_hosts))
}

/// The bearer gate for a remote bind: `Authorization: Bearer <t>` on every
/// route; the dashboard page also gets in on a first load carrying the token
/// as `?token=` and, afterwards, on the cookie the server set for it (so a
/// refresh or a bookmark doesn't have to carry the token in the URL).
/// Nothing to check on a loopback bind.
fn bearer_ok(
    ctx: &Ctx,
    method: &Method,
    url: &str,
    authorization: Option<&str>,
    cookie: Option<&str>,
) -> bool {
    if !ctx.remote {
        return true;
    }
    if let Some(supplied) = authorization.and_then(|a| a.strip_prefix("Bearer ")) {
        return same_token(supplied, &ctx.token);
    }
    if *method == Method::Get
        && url.split('?').next() == Some("/")
        && query_param(url, "token").is_some_and(|t| same_token(t, &ctx.token))
    {
        return true;
    }
    cookie
        .and_then(|c| cookie_value(c, TOKEN_COOKIE))
        .is_some_and(|t| same_token(t, &ctx.token))
}

/// One `name` from a `Cookie` header, if it's there (the first match wins).
fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name)
            .then(|| value.trim())
            .filter(|v| !v.is_empty())
    })
}

/// The allowlist entry that stands for "any `Host`": for a wide bind whose
/// name browsers will use isn't knowable (a laptop on several networks, a box
/// reached under more than one name). The bearer token still gates every
/// request; the allowlist just stops being the second lock.
const ANY_HOST: &str = "*";

fn is_any_host(entry: &str) -> bool {
    entry.trim() == ANY_HOST
}

/// Does a `Host` header match one allowlist entry exactly? An entry may name
/// a port, in which case the port must match too; a bare `host` entry allows
/// any port, like agent-deck's. `*` allows every host.
fn host_allowed(host: &str, allowed: &[String]) -> bool {
    if allowed.iter().any(|entry| is_any_host(entry)) {
        return true;
    }
    let (host, port) = split_authority(host);
    allowed.iter().any(|entry| {
        let (entry_host, entry_port) = split_authority(entry);
        host.eq_ignore_ascii_case(entry_host) && entry_port.is_none_or(|p| Some(p) == port)
    })
}

/// A request's `Origin` matching the allowlist (scheme http or https).
fn origin_allowed(origin: &str, allowed: &[String]) -> bool {
    (["http://", "https://"]).into_iter().any(|scheme| {
        origin
            .strip_prefix(scheme)
            .is_some_and(|a| host_allowed(a, allowed))
    })
}

/// An authority, `host:port` or `[h:…]:port`, split into its host and port.
/// (IPv6 without a port and with no bracket is not split.)
fn split_authority(authority: &str) -> (&str, Option<&str>) {
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            return (host, tail.strip_prefix(':').filter(|p| !p.is_empty()));
        }
        return (authority, None);
    }
    match authority.rsplit_once(':') {
        Some((host, port))
            if !host.contains(':')
                && !port.is_empty()
                && port.chars().all(|c| c.is_ascii_digit()) =>
        {
            (host, Some(port))
        }
        _ => (authority, None),
    }
}

/// Same string, without telling an attacker where it first differs: all
/// bytes are folded into one number in the same time either way.
fn same_token(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).unwrap_or(&0) ^ b.get(i).unwrap_or(&0);
    }
    diff == 0
}

/// 128 random bits from the OS, hex-encoded.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom for the session token")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Deserialize, Default)]
struct ActionBody {
    reason: Option<String>,
}

/// Runs one of the `ACTIONS` against a task, the same way `furmi <action> <id>`
/// would. `body` is the POST body, parsed for `fail`'s optional `reason`.
fn perform_action(home: &Home, id: &str, action: &str, body: &str) -> Result<()> {
    match action {
        "done" => control::done(home, Some(id.to_string())),
        "fail" => {
            let reason = serde_json::from_str::<ActionBody>(body)
                .ok()
                .and_then(|b| b.reason)
                .filter(|r| !r.trim().is_empty());
            control::fail(home, Some(id.to_string()), reason)
        }
        "cancel" => control::cancel(home, Some(id.to_string())),
        "retry" => control::retry(home, id),
        _ => unreachable!("route only matches ACTIONS"),
    }
}

/// The "New task" form.
#[derive(Deserialize)]
struct NewTaskBody {
    prompt: String,
    repo: String,
    /// `asap` (or empty), or a delay like `1h`.
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
}

/// Queues the task described by the form, as `furmi add` would. Returns its id.
fn perform_create(home: &Home, body: &str) -> Result<String> {
    let form: NewTaskBody = serde_json::from_str(body).context("couldn't read the form")?;
    let repo = form.repo.trim();
    if repo.is_empty() {
        anyhow::bail!("choose the repository the agent should work in");
    }
    // A new task has nothing to "keep": no start time means as soon as possible.
    let scheduled_at = match edit::parse_start(form.start.as_deref())? {
        Start::At(at) => Some(at),
        Start::Keep | Start::Asap => None,
    };
    let config = Config::load(&home.config_path())?;
    let spec = TaskSpec {
        repo: repo.into(),
        branch: None,
        worktree: None,
        agent: form.agent.filter(|a| !a.trim().is_empty()),
        model: None,
        mode: None,
        on_finish: form.on_finish,
        retries: None,
        retry_delay: None,
        timeout: None,
    };
    let created = create_task(
        home,
        &config,
        NewTask {
            prompt: form.prompt,
            id: None,
            spec,
            scheduled_at,
            created_by: "web".into(),
        },
    );
    match created {
        Ok((task, _)) => Ok(task.id),
        // The form shows one plain sentence, not the CLI's bulleted list.
        Err(e) => {
            let message = format!("{e:#}");
            match message.strip_prefix("invalid task:\n  - ") {
                Some(problems) => anyhow::bail!("{}", problems.replace("\n  - ", "; ")),
                None => Err(e),
            }
        }
    }
}

/// 200 with the object, or 404 with why there isn't one.
fn detail_reply(detail: Result<serde_json::Value>) -> Reply {
    match detail {
        Ok(v) => json_reply(200, v.to_string()),
        Err(e) => json_reply(404, json_err(&e)),
    }
}

/// `{"ok":true}`, or 400 with what went wrong.
fn mutation_reply(result: Result<()>) -> Reply {
    match result {
        Ok(()) => json_reply(200, "{\"ok\":true}".into()),
        Err(e) => json_reply(400, json_err(&e)),
    }
}

/// The edit form for a task that hasn't started.
#[derive(Deserialize)]
struct EditTaskBody {
    prompt: String,
    repo: String,
    /// `keep` (or empty), `asap`, or a delay like `1h`.
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
}

fn perform_edit_task(home: &Home, id: &str, body: &str) -> Result<()> {
    let form: EditTaskBody = serde_json::from_str(body).context("couldn't read the form")?;
    edit::edit_task(
        home,
        id,
        TaskEdit {
            prompt: form.prompt,
            repo: form.repo,
            start: edit::parse_start(form.start.as_deref())?,
            on_finish: form.on_finish,
            agent: form.agent,
        },
    )
}

/// The schedule form, for creating and editing alike.
#[derive(Deserialize)]
struct ScheduleBody {
    prompt: String,
    repo: String,
    cron: String,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
}

fn schedule_form(body: &str) -> Result<ScheduleEdit> {
    let form: ScheduleBody = serde_json::from_str(body).context("couldn't read the form")?;
    Ok(ScheduleEdit {
        prompt: form.prompt,
        repo: form.repo,
        cron: form.cron,
        on_finish: form.on_finish,
        agent: form.agent,
    })
}

fn perform_create_schedule(home: &Home, body: &str) -> Result<String> {
    edit::create_schedule(home, schedule_form(body)?)
}

fn perform_edit_schedule(home: &Home, id: &str, body: &str) -> Result<()> {
    edit::edit_schedule(home, id, schedule_form(body)?)
}

#[derive(Deserialize)]
struct DefaultAgentBody {
    agent: String,
}

fn perform_set_default_agent(home: &Home, body: &str) -> Result<()> {
    let form: DefaultAgentBody = serde_json::from_str(body).context("couldn't read the form")?;
    home.ensure_initialized()?;
    crate::config::set_default_agent(&home.config_path(), &form.agent)
}

/// Which coding agents are installed here and how to get the others.
fn agents_json(home: &Home) -> Result<String> {
    let config = Config::load(&home.config_path())?;
    Ok(serde_json::json!({ "agents": crate::config::agent_statuses(&config) }).to_string())
}

/// The setup check: what `furmi doctor` looks at, for the dashboard.
fn doctor_json(home: &Home) -> String {
    let checks: Vec<_> = crate::doctor::checks(home)
        .iter()
        .map(|c| serde_json::json!({ "level": c.level(), "message": c.message() }))
        .collect();
    serde_json::json!({ "checks": checks }).to_string()
}

fn json_err(e: &anyhow::Error) -> String {
    serde_json::to_string(&serde_json::json!({ "error": format!("{e:#}") }))
        .unwrap_or_else(|_| "{\"error\":\"internal error\"}".to_string())
}

/// A log reply, saying when only its end is there.
fn json_log_with(log: &str, truncated: bool) -> String {
    serde_json::to_string(&serde_json::json!({ "log": log, "truncated": truncated }))
        .unwrap_or_else(|_| "{\"log\":\"\",\"truncated\":false}".to_string())
}

/// The last `limit` bytes of a log, starting at a line boundary, and whether
/// anything was cut off.
///
/// A live log is re-read while the modal is open, and a runaway agent's log runs
/// to 64 MB — so the page asks for the tail of it rather than for the whole
/// thing every couple of seconds.
fn tail_of(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut start = text.len() - limit;
    // Never split a multi-byte character (agent output has plenty), then drop
    // the half line the cut leaves behind.
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let cut = &text[start..];
    let cut = cut.split_once('\n').map(|(_, rest)| rest).unwrap_or(cut);
    (cut.to_string(), true)
}

/// Everything the page's task view shows, for a task in any state.
///
/// `/api/tasks/<id>` is the *edit* view: it refuses anything that has already
/// started, because editing it would be a lie. This one only reads.
fn task_view_json(home: &Home, id: &str) -> Result<String> {
    crate::spec::validate_id(id).map_err(anyhow::Error::msg)?;
    let config = Config::load(&home.config_path()).unwrap_or_default();
    let task = [home.tasks(), home.archive()]
        .iter()
        .map(|dir| dir.join(format!("{id}.md")))
        .find(|path| path.exists())
        .with_context(|| format!("no task `{id}`"))
        .and_then(|path| Task::load(&path))?;
    let eff = task.spec.resolve(&config.defaults);
    let worktree = home.worktrees().join(&task.id);
    let (mut model, mut model_source) = task.model_in_use(&config);
    // A task that ran before `model_used` was kept still has its answer in the
    // log. Only here, on demand: the tables are polled every couple of seconds
    // and won't read a log per row for it.
    if model_source == "none" {
        let log = home
            .logs()
            .join(format!("{}.{}.log", task.id, task.attempts.max(1)));
        if let Some(reported) = crate::run::model_from_log(&log) {
            model = reported;
            model_source = "log";
        }
    }
    let view = serde_json::json!({
        "id": task.id,
        // The whole prompt: this is the one place the page shows it.
        "prompt": task.prompt,
        "status": task.status.as_str(),
        "repo": contract_tilde(&eff.repo),
        "branch": task.effective_branch(&config),
        "worktree": eff.worktree.then(|| contract_tilde(&worktree)).filter(|_| worktree.exists()),
        "worktree_off": !eff.worktree,
        "agent": eff.agent,
        "model": model,
        "model_source": model_source,
        "on_finish": on_finish_str(eff.on_finish),
        "mode": match eff.mode {
            crate::spec::Mode::Headless => "headless",
            crate::spec::Mode::Interactive => "interactive",
        },
        "created_at": task.created_at.map(fmt_time),
        "scheduled_at": task.scheduled_at.map(fmt_time),
        "started_at": task.started_at.map(fmt_time),
        "finished_at": task.finished_at.map(fmt_time),
        "attempts": task.attempts,
        "interruptions": task.interruptions,
        "exit_code": task.exit_code,
        "error": task.error,
        "tokens_in": task.tokens_in,
        "tokens_out": task.tokens_out,
        "cost_usd": task.cost_usd,
    });
    Ok(view.to_string())
}

fn query_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

#[derive(Serialize)]
struct Row {
    id: String,
    status: &'static str,
    when: String,
    /// The instant behind `when`, RFC 3339 (and `None` when there is none:
    /// "as soon as possible"). The page sorts and time-filters on this and
    /// ticks the "running for" clock from it, since the printed form has
    /// minute precision and its own idea of "asap".
    ///
    /// Token/cost usage deliberately isn't here: it only exists for completed
    /// headless runs of agents that print JSON, so a column of it would be
    /// mostly dashes and easy to mistake for accounting. `furmi ls` and the task
    /// files still have it.
    when_at: Option<String>,
    repo: String,
    branch: String,
    agent: String,
    /// The model this task is about: what it actually ran with once it has
    /// run (its own `model_used`, from the agent's output), else the model it
    /// pins, else the one its agent is configured with. `"-"` when nothing
    /// says, e.g. an agent on its own default. Its own field rather than glued
    /// onto the agent because real model names are long (`hf.co/unsloth/\
    /// Qwen3.6-35B-A3B-GGUF:UD-Q4_K_XL`).
    model: String,
    /// Which of those `model` came from, so the page can say where it got it:
    /// `used` (the agent reported it), `task` (the task pins it), `agent` (the
    /// agent's configured default) or `none`.
    model_source: &'static str,
    /// What the task asks for, on one line and short: enough to search it and
    /// to tell two tasks in the same repo apart without opening either.
    summary: String,
    /// Why a failed (or otherwise unfinished) task ended up that way.
    error: Option<String>,
}

#[derive(Serialize)]
struct ScheduleRow {
    id: String,
    /// The first line of what it asks the agent to do.
    summary: String,
    enabled: bool,
    cron: String,
    next: String,
    repo: String,
}

/// A task, schedule or config file that couldn't be read.
#[derive(Serialize)]
struct InvalidRow {
    file: String,
    error: String,
}

#[derive(Serialize)]
struct RunnerRow {
    alive: bool,
    last_tick: Option<String>,
}

#[derive(Serialize, Default)]
struct Counts {
    pending: usize,
    running: usize,
    done: usize,
    failed: usize,
    cancelled: usize,
    needs_review: usize,
    schedules: usize,
    invalid: usize,
}

/// What the "New task" form offers and pre-selects.
#[derive(Serialize)]
struct FormInfo {
    /// Repos to suggest, most relevant first.
    repos: Vec<String>,
    agents: Vec<String>,
    /// The default agent, if one is set.
    agent: Option<String>,
    on_finish: &'static str,
}

#[derive(Serialize)]
struct State {
    home: String,
    generated_at: String,
    counts: Counts,
    queue: Vec<Row>,
    running: Vec<Row>,
    archive: Vec<Row>,
    schedules: Vec<ScheduleRow>,
    invalid: Vec<InvalidRow>,
    runner: RunnerRow,
    form: FormInfo,
}

/// Loads every file in `paths`, setting aside the ones that don't parse
/// instead of failing the whole dashboard over one bad file.
fn load_all<T>(
    paths: Vec<std::path::PathBuf>,
    load: impl Fn(&std::path::Path) -> Result<T>,
    invalid: &mut Vec<InvalidRow>,
) -> Vec<T> {
    let mut ok = Vec::new();
    for path in paths {
        match load(&path) {
            Ok(v) => ok.push(v),
            Err(e) => invalid.push(InvalidRow {
                file: contract_tilde(&path),
                error: format!("{e:#}"),
            }),
        }
    }
    ok
}

fn state_json(home: &Home, cwd_repo: Option<&str>) -> Result<String> {
    let mut invalid = Vec::new();
    let config = Config::load(&home.config_path()).unwrap_or_else(|e| {
        invalid.push(InvalidRow {
            file: contract_tilde(&home.config_path()),
            error: format!("{e:#} (showing built-in defaults)"),
        });
        Config::default()
    });
    let mut counts = Counts::default();

    let (mut running_tasks, mut queue_tasks): (Vec<_>, Vec<_>) =
        load_all(md_files(&home.tasks())?, Task::load, &mut invalid)
            .into_iter()
            .partition(|t| t.status == Status::Running);
    queue_tasks.sort_by_key(|t| (t.scheduled_at.or(t.created_at), t.id.clone()));
    running_tasks.sort_by_key(|t| (t.started_at, t.id.clone()));
    counts.pending = queue_tasks.len();
    counts.running = running_tasks.len();

    let mut archive_tasks = load_all(md_files(&home.archive())?, Task::load, &mut invalid);
    for t in &archive_tasks {
        match t.status {
            Status::Done => counts.done += 1,
            Status::Failed => counts.failed += 1,
            Status::Cancelled => counts.cancelled += 1,
            Status::NeedsReview => counts.needs_review += 1,
            Status::Pending | Status::Running => {}
        }
    }
    archive_tasks.sort_by_key(|t| Reverse(t.finished_at));
    let form = form_info(
        &config,
        cwd_repo,
        &[&running_tasks, &queue_tasks, &archive_tasks],
    );

    let schedules: Vec<ScheduleRow> =
        load_all(md_files(&home.schedules())?, Schedule::load, &mut invalid)
            .into_iter()
            .map(|s| {
                let next = match s.next_after(chrono::Local::now()) {
                    Ok(t) if s.enabled => fmt_time(t.fixed_offset()),
                    Ok(_) => "-".into(),
                    Err(_) => "invalid".into(),
                };
                ScheduleRow {
                    id: s.id.clone(),
                    summary: s.prompt.lines().next().unwrap_or_default().to_string(),
                    enabled: s.enabled,
                    cron: s.cron.clone(),
                    next,
                    repo: contract_tilde(&s.spec.resolve(&config.defaults).repo),
                }
            })
            .collect();
    counts.schedules = schedules.len();
    counts.invalid = invalid.len();

    let state = State {
        home: contract_tilde(home.root()),
        generated_at: fmt_time(now()),
        counts,
        queue: queue_tasks.iter().map(|t| to_row(t, &config)).collect(),
        running: running_tasks.iter().map(|t| to_row(t, &config)).collect(),
        archive: archive_tasks.iter().map(|t| to_row(t, &config)).collect(),
        schedules,
        invalid,
        runner: RunnerRow {
            alive: home.runner_alive(),
            last_tick: RunnerState::load(home)
                .and_then(|s| s.last_tick_at)
                .map(fmt_time),
        },
        form,
    };
    Ok(serde_json::to_string(&state)?)
}

/// Repos from the folder `furmi web` started in, then from recent tasks (newest
/// first, each once), plus the configured agents and finish action.
fn form_info(config: &Config, cwd_repo: Option<&str>, tasks: &[&Vec<Task>]) -> FormInfo {
    let mut recent: Vec<&Task> = tasks.iter().flat_map(|t| t.iter()).collect();
    recent.sort_by_key(|t| Reverse(t.created_at));
    let mut repos: Vec<String> = Vec::new();
    for repo in cwd_repo.map(str::to_string).into_iter().chain(
        recent
            .iter()
            .map(|t| contract_tilde(&t.spec.resolve(&config.defaults).repo)),
    ) {
        if !repos.contains(&repo) {
            repos.push(repo);
        }
    }
    // Only agents that can actually run here (the default always counts), so
    // the form doesn't offer a choice that would just fail.
    let agents = config
        .agents
        .iter()
        .filter(|(name, agent)| {
            config.defaults.agent.as_deref() == Some(name.as_str())
                || agent.headless.first().is_some_and(|p| on_path(p))
        })
        .map(|(name, _)| name.clone())
        .collect();
    FormInfo {
        repos,
        agents,
        agent: config.defaults.agent.clone(),
        on_finish: edit::on_finish_str(config.defaults.on_finish),
    }
}

fn to_row(t: &Task, config: &Config) -> Row {
    let eff = t.spec.resolve(&config.defaults);
    let at = match (t.finished_at, t.started_at, t.scheduled_at) {
        (Some(at), _, _) | (None, Some(at), _) | (None, None, Some(at)) => Some(at),
        (None, None, None) => None,
    };
    let (model, model_source) = t.model_in_use(config);
    Row {
        id: t.id.clone(),
        status: t.status.as_str(),
        when: at.map(fmt_time).unwrap_or_else(|| "asap".into()),
        when_at: at.map(|at| at.to_rfc3339()),
        repo: contract_tilde(&eff.repo),
        branch: t.effective_branch(config).unwrap_or_else(|| "-".into()),
        agent: eff.agent.clone().unwrap_or_else(|| "-".into()),
        summary: summary_of(&t.prompt),
        model,
        model_source,
        error: t.error.clone().filter(|e| !e.trim().is_empty()),
    }
}

/// A task's prompt on one line, cut short: the first non-empty line of it,
/// which for a task written like a message is the ask.
fn summary_of(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let line = line.trim_start_matches(['#', '-', '*', '>', ' ']);
    let mut summary: String = line.chars().take(120).collect();
    if summary.len() < line.len() {
        summary = summary.trim_end().to_string();
        summary.push('…');
    }
    summary
}

fn fmt_time(t: chrono::DateTime<chrono::FixedOffset>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 0600 token file, the way `furmi web --setup` makes one.
    fn token_file(dir: &std::path::Path, token: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("web-token");
        std::fs::write(&path, format!("{token}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn args(listen: Option<&str>, hosts: &[&str]) -> WebArgs {
        WebArgs {
            port: 7433,
            listen: listen.map(str::to_string),
            token_file: None,
            token: None,
            allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
            setup: false,
            replace: false,
            no_open: true,
            detach: false,
        }
    }

    #[test]
    fn flags_beat_the_config_which_beats_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let saved_token = token_file(dir.path(), "saved");
        let saved = Web {
            configured: true,
            listen: Some("0.0.0.0:9000".into()),
            allowed_hosts: vec!["saved.ts.net".into()],
            token_file: Some(saved_token.clone()),
        };

        // Nothing configured: loopback, and no shared secret needed.
        let s = resolve(&args(None, &[]), &Web::default()).unwrap();
        assert_eq!(s.addr, "127.0.0.1:7433");
        assert!(!s.remote);
        assert!(s.bearer.is_none());

        // The saved answers, with no flags at all: wide and gated.
        let s = resolve(&args(None, &[]), &saved).unwrap();
        assert_eq!(s.addr, "0.0.0.0:9000");
        assert!(s.remote);
        assert_eq!(s.bearer.as_deref(), Some("saved"));
        assert_eq!(s.allowed_hosts, ["saved.ts.net"]);
        assert!(s.token_from.unwrap().ends_with("web-token"));

        // A flag overrides what the file says, per setting.
        let mut override_args = args(Some("0.0.0.0:1234"), &["flag.ts.net"]);
        override_args.token = Some("from-flag".into());
        let s = resolve(&override_args, &saved).unwrap();
        assert_eq!(s.addr, "0.0.0.0:1234");
        assert_eq!(s.bearer.as_deref(), Some("from-flag"));
        assert_eq!(s.allowed_hosts, ["flag.ts.net"]);
    }

    #[test]
    fn a_busy_port_is_told_apart_from_other_bind_failures() {
        // Something else holds the port: that's the one worth offering to
        // replace.
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = held.local_addr().unwrap().to_string();
        match Server::http(busy.as_str()) {
            Ok(_) => panic!("bound {busy}, which was meant to be taken"),
            Err(e) => assert!(address_in_use(&*e), "{e}"),
        }

        // A nonsense address fails for another reason, and isn't offered.
        match Server::http("127.0.0.1:99999") {
            Ok(_) => panic!("a nonsense port was accepted"),
            Err(e) => assert!(!address_in_use(&*e), "{e}"),
        }

        // The same port on another interface or address is still a clash.
        assert!(same_port("0.0.0.0:7433", "127.0.0.1:7433"));
        assert!(same_port("[::]:7433", "0.0.0.0:7433"));
        assert!(!same_port("0.0.0.0:7433", "0.0.0.0:8080"));
        assert!(!same_port("box.ts.net", "box.ts.net:7433"));
    }

    #[test]
    fn a_broken_config_still_serves_loopback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tasks")).unwrap();
        std::fs::write(dir.path().join("config.toml"), "this is [not toml").unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        let serving = setup(&home, &args(None, &[])).unwrap();
        assert_eq!(serving.addr, "127.0.0.1:7433");
        assert!(!serving.remote);
    }

    #[test]
    fn a_port_less_listen_takes_the_port_flag() {
        let web = Web {
            listen: Some("0.0.0.0".into()),
            ..Web::default()
        };
        let mut with_port = args(None, &[]);
        with_port.port = 8080;
        assert_eq!(addr_of(&with_port, &web), "0.0.0.0:8080");
        assert_eq!(addr_of(&args(Some("1.2.3.4:9"), &[]), &web), "1.2.3.4:9");
        assert_eq!(addr_of(&args(None, &[]), &Web::default()), "127.0.0.1:7433");
    }

    #[test]
    fn a_fresh_home_is_asked_once_however_it_serves() {
        // Nothing answered yet: a first run asks, even for loopback.
        assert!(never_asked(&Web::default()));
        // Any answer at all is an answer — including "this machine only".
        assert!(!never_asked(&Web {
            configured: true,
            ..Web::default()
        }));
        // A hand-written table is never asked about.
        assert!(!never_asked(&Web {
            listen: Some("0.0.0.0:7433".into()),
            ..Web::default()
        }));
        assert!(!never_asked(&Web {
            allowed_hosts: vec!["box.ts.net".into()],
            ..Web::default()
        }));
    }

    #[test]
    fn setup_is_asked_only_while_a_wide_bind_lacks_its_answers() {
        // Loopback never needs answering.
        assert!(!unanswered(&args(None, &[]), &Web::default()));
        // Wide with nothing: both answers missing.
        assert!(unanswered(
            &args(Some("0.0.0.0:7433"), &[]),
            &Web::default()
        ));
        // A wide address saved in config counts just the same.
        let wide = Web {
            listen: Some("0.0.0.0:7433".into()),
            ..Web::default()
        };
        assert!(unanswered(&args(None, &[]), &wide));
        // One of the two given is still half an answer.
        assert!(unanswered(
            &args(Some("0.0.0.0:7433"), &["box.ts.net"]),
            &Web::default()
        ));
        // Both given by flags: no questions.
        let mut answered = args(Some("0.0.0.0:7433"), &["box.ts.net"]);
        answered.token = Some("t".into());
        assert!(!unanswered(&answered, &Web::default()));
    }

    #[test]
    fn hosts_and_urls_are_cleaned_up_for_the_browser() {
        assert_eq!(
            parse_hosts(" box.ts.net. , 100.64.0.10 ,BOX.ts.net"),
            ["box.ts.net", "100.64.0.10"]
        );
        assert!(parse_hosts("  ,  ").is_empty());

        // The port comes from the allowlist entry, else from the bind.
        assert_eq!(
            url_for("box.ts.net", "0.0.0.0:7433"),
            "http://box.ts.net:7433"
        );
        assert_eq!(
            url_for("box.ts.net:9000", "0.0.0.0:7433"),
            "http://box.ts.net:9000"
        );
        assert_eq!(url_for("box.ts.net", "0.0.0.0:80"), "http://box.ts.net");
        assert_eq!(url_for("[fd7a::1]", "[::]:7433"), "http://[fd7a::1]:7433");
    }

    #[test]
    fn the_tailnet_name_comes_out_of_tailscales_status() {
        // The shape `tailscale status --json` prints (trimmed): the MagicDNS
        // name carries a trailing dot, and the IPv4 is the first address.
        let status = br#"{"Self":{"DNSName":"box.tailnet.ts.net.","TailscaleIPs":["100.64.0.10","fd7a::1"]}}"#;
        match parse_tailnet_status(status) {
            TailnetLookup::Found(tailnet) => {
                assert_eq!(tailnet.name, "box.tailnet.ts.net");
                assert_eq!(tailnet.ip.as_deref(), Some("100.64.0.10"));
                assert_eq!(tailnet.hosts(), ["box.tailnet.ts.net", "100.64.0.10"]);
            }
            TailnetLookup::Unavailable(why) => panic!("expected a tailnet, got {why}"),
        }

        // Not logged in: nothing named, and it says so.
        for json in [
            br#"{"Self":{}}"#.as_slice(),
            br#"{"Self":{"DNSName":""}}"#.as_slice(),
            br#"{"BackendState":"NeedsLogin"}"#.as_slice(),
        ] {
            match parse_tailnet_status(json) {
                TailnetLookup::Found(_) => panic!("expected nothing for {:?}", json as &[u8]),
                TailnetLookup::Unavailable(why) => assert!(!why.is_empty()),
            }
        }

        // Garbage in, nothing out — and never a panic.
        match parse_tailnet_status(b"not json") {
            TailnetLookup::Unavailable(why) => assert!(why.contains("usable"), "{why}"),
            TailnetLookup::Found(_) => panic!("expected nothing"),
        }
    }

    #[test]
    fn a_star_allows_any_host() {
        let any = vec!["*".to_string()];
        assert!(host_allowed("box.ts.net", &any));
        assert!(host_allowed("box.ts.net:7433", &any));
        assert!(host_allowed("100.64.0.10:7433", &any));
        assert!(host_allowed("anything.at.all", &any));
        // ...for the POST Origin check too, which shares the rule.
        assert!(origin_allowed("http://anything.at.all", &any));
        // And it's only reachable through the allowlist: a loopback bind still
        // checks for a loopback Host.
        assert!(!host_allowed("evil.example", &["box.ts.net".to_string()]));
    }

    #[test]
    fn the_open_url_keeps_the_token_off_non_terminals() {
        let serving = Serving {
            addr: "0.0.0.0:7433".into(),
            remote: true,
            bearer: Some("secret".into()),
            allowed_hosts: vec!["box.ts.net".into()],
            token_from: Some("~/.furmiculi/web-token".into()),
        };
        assert_eq!(
            serving.open_url("secret", true).unwrap(),
            "http://box.ts.net:7433/?token=secret"
        );
        assert_eq!(
            serving.open_url("secret", false).unwrap(),
            "http://box.ts.net:7433/?token=<the token>"
        );

        // Any Host: no name to print, so say where to put one — never a
        // literal `http://*`.
        let any = Serving {
            allowed_hosts: vec!["*".into()],
            ..serving
        };
        assert_eq!(
            any.open_url("secret", false).unwrap(),
            "http://<this box's tailnet name or IP>:7433/?token=<the token>"
        );
    }

    #[test]
    fn only_loopback_hosts_and_origins_pass() {
        for ok in ["127.0.0.1", "127.0.0.1:7433", "localhost:80", "[::1]:7433"] {
            assert!(is_loopback_authority(ok), "{ok}");
        }
        for bad in [
            "evil.com",
            "evil.com:7433",
            "127.0.0.1.evil.com",
            "",
            "10.0.0.5:7433",
        ] {
            assert!(!is_loopback_authority(bad), "{bad}");
        }
        assert!(is_loopback_origin("http://localhost:7433"));
        assert!(!is_loopback_origin("https://localhost:7433"));
        assert!(!is_loopback_origin("http://evil.com"));
        assert!(!is_loopback_origin("null"));
    }

    #[test]
    fn a_failed_tasks_reason_reaches_its_row() {
        let spec = TaskSpec {
            repo: "/tmp".into(),
            branch: None,
            worktree: None,
            agent: Some("claude".into()),
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
        };
        let mut task = Task::new("t".into(), spec, "do it".into());
        let config = Config::default();
        assert_eq!(to_row(&task, &config).error, None);

        task.status = Status::Failed;
        task.error = Some("exit code 1".into());
        assert_eq!(to_row(&task, &config).error.as_deref(), Some("exit code 1"));

        // A provider/model failure reaches the dashboard with its reason.
        task.error = Some("the agent's provider rejected the run: model is not available".into());
        assert!(
            to_row(&task, &config)
                .error
                .unwrap()
                .contains("provider rejected the run")
        );

        task.error = Some("  ".into());
        assert_eq!(to_row(&task, &config).error, None);
    }

    #[test]
    fn a_row_carries_the_time_the_page_sorts_on() {
        let spec = TaskSpec {
            repo: "/tmp".into(),
            branch: None,
            worktree: None,
            agent: None,
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
        };
        let mut task = Task::new("t".into(), spec, "do it".into());
        let config = Config::default();

        // Waiting with no start time: nothing to sort on or count down from.
        let row = to_row(&task, &config);
        assert_eq!(row.when, "asap");
        assert_eq!(row.when_at, None);
        assert_eq!(row.agent, "-");
        assert_eq!(row.model, "-");

        // Running: a real timestamp, in a form the page can parse, and the
        // agent and model each on their own (the page shows them side by side).
        task.status = Status::Running;
        task.started_at = Some(now());
        task.spec.agent = Some("pi".into());
        task.spec.model = Some("hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-Q4_K_XL".into());
        let row = to_row(&task, &config);
        assert_eq!(row.agent, "pi");
        assert_eq!(row.model, "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-Q4_K_XL");
        let at = row.when_at.expect("a running task knows when it started");
        assert!(chrono::DateTime::parse_from_rfc3339(&at).is_ok(), "{at}");
    }

    #[test]
    fn the_model_column_names_what_ran_and_says_where_it_got_the_name() {
        let spec = TaskSpec {
            repo: "/tmp".into(),
            branch: None,
            worktree: None,
            agent: None,
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
        };
        let mut task = Task::new("t".into(), spec, "do it".into());
        let mut config = Config::default();

        // Nothing anywhere: nothing to name.
        assert_eq!(to_row(&task, &config).model, "-");
        assert_eq!(to_row(&task, &config).model_source, "none");

        // The agent's configured model is the answer when the task pins none.
        config.agents.insert(
            "pi".into(),
            crate::config::Agent {
                headless: Vec::new(),
                interactive: Vec::new(),
                model_args: Vec::new(),
                model: Some("agent/default".into()),
            },
        );
        config.defaults.agent = Some("pi".into());
        let row = to_row(&task, &config);
        assert_eq!(row.model, "agent/default");
        assert_eq!(row.model_source, "agent");

        // A task that pins one says so, overriding the agent's.
        task.spec.model = Some("pinned/model".into());
        let row = to_row(&task, &config);
        assert_eq!(row.model, "pinned/model");
        assert_eq!(row.model_source, "task");

        // Still waiting: what it *will* run with is the pinned model, even if
        // an earlier attempt reported its own.
        task.model_used = Some("reported/model".into());
        assert_eq!(to_row(&task, &config).model, "pinned/model");

        // Finished: the agent's own answer is the truth, model_used wins.
        task.status = Status::Done;
        task.finished_at = Some(now());
        let row = to_row(&task, &config);
        assert_eq!(row.model, "reported/model");
        assert_eq!(row.model_source, "used");

        // A finished task with no report of its own falls back to the pin.
        task.model_used = None;
        assert_eq!(to_row(&task, &config).model, "pinned/model");
    }

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn token_equality_is_exact_and_whole_string() {
        assert!(same_token("abc", "abc"));
        assert!(!same_token("abc", "abd"));
        assert!(!same_token("abc", "abcd"));
        assert!(!same_token("abcd", "abc"));
        assert!(!same_token("", "a"));
        assert!(same_token("", ""));
    }

    #[test]
    fn hosts_match_allowlist_entries_exactly() {
        let allowed = vec![
            "toolnet.example".to_string(),
            "dash.example:8443".to_string(),
        ];
        // A bare-entry host, any port.
        for ok in [
            "toolnet.example",
            "toolnet.example:7433",
            "toolnet.example:1",
        ] {
            assert!(host_allowed(ok, &allowed), "{ok}");
        }
        // An entry with a port: the port must match too.
        for bad in [
            "dash.example:9",
            "dash.example",
            "evil.example",
            "toolnet.evil.example",
            "eviltoolnet.example",
            "",
            "toolnet.example.evil.com",
        ] {
            assert!(!host_allowed(bad, &allowed), "{bad}");
        }
        // An empty allowlist lets nothing through.
        assert!(!host_allowed("toolnet.example", &[]));
    }

    #[test]
    fn origins_match_allowlist_entries_with_a_scheme() {
        let allowed = vec!["toolnet.example".to_string()];
        assert!(origin_allowed("http://toolnet.example:7433", &allowed));
        assert!(origin_allowed("https://toolnet.example", &allowed));
        assert!(!origin_allowed("http://evil.example", &allowed));
        assert!(!origin_allowed("null", &allowed));
        // No scheme, no match.
        assert!(!origin_allowed("toolnet.example", &allowed));
    }

    #[test]
    fn authorities_split_into_host_and_port() {
        assert_eq!(
            split_authority("127.0.0.1:7433"),
            ("127.0.0.1", Some("7433"))
        );
        assert_eq!(
            split_authority("toolnet.example"),
            ("toolnet.example", None)
        );
        assert_eq!(
            split_authority("toolnet.example:8443"),
            ("toolnet.example", Some("8443"))
        );
        assert_eq!(split_authority("[::1]:7433"), ("::1", Some("7433")));
        assert_eq!(split_authority("[::1]"), ("::1", None));
        assert_eq!(split_authority("::1"), ("::1", None));
        assert_eq!(split_authority(""), ("", None));
    }

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

    #[test]
    fn a_non_loopback_listen_refuses_to_run_without_a_token() {
        let args = WebArgs {
            port: 7433,
            listen: Some("0.0.0.0:7433".into()),
            token_file: None,
            token: None,
            allowed_hosts: vec![],
            setup: false,
            replace: false,
            no_open: true,
            detach: false,
        };
        let none = Web::default();
        let err = resolve(&args, &none).unwrap_err().to_string();
        assert!(err.contains("token"), "{err}");
        assert!(err.contains("--token-file"), "{err}");
        assert!(err.contains("--setup"), "{err}");

        // ...and without an allowlist, even with a token in hand.
        let args = WebArgs {
            token: Some("t".into()),
            allowed_hosts: vec![],
            ..args
        };
        let err = resolve(&args, &none).unwrap_err().to_string();
        assert!(err.contains("--allowed-host"), "{err}");

        // With both, it's a remote serving setup.
        let args = WebArgs {
            allowed_hosts: vec!["toolnet.example".into()],
            ..args
        };
        let serving = resolve(&args, &none).unwrap();
        assert!(serving.remote);
        assert_eq!(serving.bearer.as_deref(), Some("t"));

        // The default port stays loopback, token or none.
        let loopback = resolve(
            &WebArgs {
                listen: None,
                allowed_hosts: vec!["toolnet.example".into()],
                ..args
            },
            &none,
        )
        .unwrap();
        assert!(!loopback.remote);
    }

    #[test]
    fn the_token_cookie_is_found_among_the_others() {
        assert_eq!(cookie_value("furmi_token=abc", TOKEN_COOKIE), Some("abc"));
        assert_eq!(
            cookie_value("other=1; furmi_token=abc; third=2", TOKEN_COOKIE),
            Some("abc")
        );
        assert_eq!(
            cookie_value(" furmi_token = abc ", TOKEN_COOKIE),
            Some("abc")
        );
        assert_eq!(cookie_value("other=1", TOKEN_COOKIE), None);
        assert_eq!(cookie_value("furmi_token=", TOKEN_COOKIE), None);
        assert_eq!(cookie_value("furmi_token", TOKEN_COOKIE), None);
        assert_eq!(cookie_value("furmi_tokens=abc", TOKEN_COOKIE), None);
        assert_eq!(cookie_value("", TOKEN_COOKIE), None);
    }

    #[test]
    fn query_param_finds_and_misses() {
        assert_eq!(
            query_param("/api/tasks/x/logs?attempt=2", "attempt"),
            Some("2")
        );
        assert_eq!(query_param("/api/tasks/x/logs?a=1&b=2", "b"), Some("2"));
        assert_eq!(query_param("/api/tasks/x/logs", "attempt"), None);
        assert_eq!(query_param("/api/tasks/x/logs?a=1", "attempt"), None);
    }

    #[test]
    fn action_body_parses_reason_or_defaults() {
        let with: ActionBody = serde_json::from_str(r#"{"reason":"bad approach"}"#).unwrap();
        assert_eq!(with.reason.as_deref(), Some("bad approach"));

        let empty: ActionBody = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.reason, None);
    }

    #[test]
    fn json_err_and_json_log_produce_valid_json() {
        let err = json_err(&anyhow::anyhow!("boom"));
        let v: serde_json::Value = serde_json::from_str(&err).unwrap();
        assert_eq!(v["error"], "boom");

        let log = json_log_with("line one\nline two", false);
        let v: serde_json::Value = serde_json::from_str(&log).unwrap();
        assert_eq!(v["log"], "line one\nline two");
        assert_eq!(v["truncated"], false);

        let long = "a".repeat(100) + "," + &"b".repeat(100);
        let (tail, cut) = tail_of(&long, 100);
        assert!(cut);
        assert!(long.ends_with(&tail));
        assert!(!tail.contains('\n'), "a cut log starts at a line");
        assert_eq!(tail.len(), 100);
        // Nothing to cut: the whole thing comes back untouched.
        assert_eq!(tail_of("short", 100), ("short".to_string(), false));
        // A cut inside a multi-byte character moves forward to a whole one.
        let wide = format!("{}\n{}", "é".repeat(60), "ü".repeat(60));
        let (tail, cut) = tail_of(&wide, 61);
        assert!(cut);
        assert!(!tail.contains('\u{fffd}'));
        assert!("éü".contains(tail.chars().next().unwrap()));
    }
}
