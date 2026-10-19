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

use std::cmp::Reverse;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::cmd::{NewTask, create_task, md_files};
use crate::config::{Config, on_path};
use crate::control;
use crate::edit::{self, ScheduleEdit, Start, TaskEdit};
use crate::home::{Home, contract_tilde};
use crate::run::{self, RunnerState};
use crate::schedule::Schedule;
use crate::spec::{OnFinish, TaskSpec};
use crate::task::{Status, Task, fmt_cost, fmt_tokens, now};

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
    /// non-loopback `--listen` binds. Repeatable. `host` or `host:port`.
    #[arg(long = "allowed-host")]
    allowed_hosts: Vec<String>,
    /// Don't open the dashboard in a browser.
    #[arg(long)]
    no_open: bool,
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
}

fn reply(status: u16, content_type: &'static str, body: impl Into<String>) -> Reply {
    Reply {
        status,
        content_type,
        body: body.into(),
    }
}

fn json_reply(status: u16, body: String) -> Reply {
    reply(status, "application/json", body)
}

pub fn serve(home: &Home, args: WebArgs) -> Result<()> {
    home.ensure_initialized()?;
    let serving = setup(&args)?;
    let server = bind(&serving)?;
    run_server(home, server, &serving, &args)
}

/// `furmi start`: the runner and the dashboard in one command. The runner
/// stops when the dashboard does, so there's nothing to clean up.
pub fn start(home: &Home, args: WebArgs) -> Result<()> {
    home.ensure_initialized()?;
    let serving = setup(&args)?;
    let server = bind(&serving)?;
    let _runner = if home.runner_alive() {
        println!("The runner is already running.");
        None
    } else {
        Some(spawn_runner(home)?)
    };
    run_server(home, server, &serving, &args)
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
}

fn setup(args: &WebArgs) -> Result<Serving> {
    let addr = args
        .listen
        .clone()
        .unwrap_or_else(|| format!("127.0.0.1:{}", args.port));
    let remote = !is_loopback_authority(host_of(&addr));
    let bearer = match (&args.token_file, &args.token) {
        (Some(path), None) => Some(read_token_file(path)?),
        (None, Some(t)) => Some(t.clone()),
        (Some(_), Some(_)) => {
            anyhow::bail!("give the token one way: --token-file or --token, not both")
        }
        (None, None) => None,
    };
    if remote {
        let Some(bearer) = bearer else {
            anyhow::bail!(
                "{addr} is not a loopback bind: browsers on other machines could reach it, \
                 so it needs a shared secret. Give one with --token-file <file> (preferred) \
                 or --token <value>, and set --allowed-host to the names those browsers use"
            );
        };
        if args.allowed_hosts.is_empty() {
            anyhow::bail!(
                "{addr} is not a loopback bind: without --allowed-host no Host header \
                 could ever be trusted, so every request would be rejected. Name the \
                 hosts that should be allowed, e.g. --allowed-host tools.example"
            );
        }
        Ok(Serving {
            addr,
            remote: true,
            bearer: Some(bearer),
            allowed_hosts: args.allowed_hosts.clone(),
        })
    } else {
        Ok(Serving {
            addr,
            remote: false,
            bearer: None,
            allowed_hosts: Vec::new(),
        })
    }
}

/// Reads the token file: one line, trimmed. It should be 0600; a group- or
/// world-readable one is refused, the way a shared secret shouldn't sit.
fn read_token_file(path: &std::path::Path) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
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

fn bind(serving: &Serving) -> Result<Server> {
    let addr = &serving.addr;
    Server::http(addr.as_str()).map_err(|e| {
        anyhow::anyhow!(
            "couldn't listen on {addr}: {e}. If `furmi web` or `furmi start` is already running, \
             open http://{addr}; otherwise pick another address with --listen"
        )
    })
}

fn run_server(home: &Home, server: Server, serving: &Serving, args: &WebArgs) -> Result<()> {
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
    println!("Dashboard: {url}  (Ctrl-C to stop)");
    if serving.remote {
        println!(
            "Serving other machines: every request needs the bearer token, and a Host of {}.",
            serving
                .allowed_hosts
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        );
    }
    if !args.no_open && std::io::stdout().is_terminal() {
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
    let mut response = Response::from_string(result.body).with_status_code(result.status);
    let mut add = |name: &str, value: &str| {
        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(h);
        }
    };
    add("Content-Type", result.content_type);
    add("Cache-Control", "no-store");
    add("X-Content-Type-Options", "nosniff");
    if is_index {
        add("X-Frame-Options", "DENY");
        add(
            "Content-Security-Policy",
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             connect-src 'self'; frame-ancestors 'none'",
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
            ["api", "schedules", id] => detail_reply(edit::schedule_detail(home, id)),
            ["api", "tasks", id, "logs"] => {
                let attempt = query_param(url, "attempt").and_then(|v| v.parse().ok());
                match control::logs(home, id, attempt) {
                    Ok(log) => json_reply(200, json_log(&log)),
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
fn is_loopback_authority(authority: &str) -> bool {
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

/// Does a `Host` header match one allowlist entry exactly? An entry may name
/// a port, in which case the port must match too; a bare `host` entry allows
/// any port, like agent-deck's.
fn host_allowed(host: &str, allowed: &[String]) -> bool {
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

fn json_log(log: &str) -> String {
    serde_json::to_string(&serde_json::json!({ "log": log }))
        .unwrap_or_else(|_| "{\"log\":\"\"}".to_string())
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
    repo: String,
    branch: String,
    agent: String,
    tokens: String,
    cost: String,
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
    let when = match (t.finished_at, t.started_at, t.scheduled_at) {
        (Some(at), _, _) | (None, Some(at), _) | (None, None, Some(at)) => fmt_time(at),
        _ => "asap".into(),
    };
    Row {
        id: t.id.clone(),
        status: t.status.as_str(),
        when,
        repo: contract_tilde(&eff.repo),
        branch: t.effective_branch(config).unwrap_or_else(|| "-".into()),
        agent: match (eff.agent, eff.model) {
            (Some(a), Some(m)) => format!("{a}/{m}"),
            (Some(a), None) => a,
            (None, _) => "-".into(),
        },
        tokens: fmt_tokens(t.tokens_in, t.tokens_out),
        cost: fmt_cost(t.cost_usd),
        error: t.error.clone().filter(|e| !e.trim().is_empty()),
    }
}

fn fmt_time(t: chrono::DateTime<chrono::FixedOffset>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_non_loopback_listen_refuses_to_run_without_a_token() {
        let args = WebArgs {
            port: 7433,
            listen: Some("0.0.0.0:7433".into()),
            token_file: None,
            token: None,
            allowed_hosts: vec![],
            no_open: true,
        };
        let err = setup(&args).unwrap_err().to_string();
        assert!(err.contains("token"), "{err}");
        assert!(err.contains("--token-file"), "{err}");

        // ...and without an allowlist, even with a token in hand.
        let args = WebArgs {
            token: Some("t".into()),
            allowed_hosts: vec![],
            ..args
        };
        let err = setup(&args).unwrap_err().to_string();
        assert!(err.contains("--allowed-host"), "{err}");

        // With both, it's a remote serving setup.
        let args = WebArgs {
            allowed_hosts: vec!["toolnet.example".into()],
            ..args
        };
        let serving = setup(&args).unwrap();
        assert!(serving.remote);
        assert_eq!(serving.bearer.as_deref(), Some("t"));

        // The default port stays loopback, token or none.
        let loopback = setup(&WebArgs {
            listen: None,
            allowed_hosts: vec!["toolnet.example".into()],
            ..args
        })
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

        let log = json_log("line one\nline two");
        let v: serde_json::Value = serde_json::from_str(&log).unwrap();
        assert_eq!(v["log"], "line one\nline two");
    }
}
