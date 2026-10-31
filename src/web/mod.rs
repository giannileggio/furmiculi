//! `furmi web`: a Sidekiq-style dashboard over the queue, with actions.
//!
//! Serves one HTML shell, two Server-Sent Events streams that push what
//! changes (`/api/events` the page's state, `/api/tasks/<id>/logs/stream` a
//! running task's log), `/api/state` and `/api/tasks/<id>/logs` for the same
//! data as plain requests, `/api/tasks/<id>/{done,fail,cancel,retry}`
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
//!
//! Below, the dashboard is split the way its parts divide: `setup` the flags
//! and the saved answers, `auth` who may talk to it, `routes` and `handlers`
//! what it answers, `state` the page's data, `tailnet` the host names to
//! suggest, and `bind`/`detach` where and how it runs.

pub(crate) mod auth;
mod bind;
mod detach;
mod events;
mod handlers;
mod routes;
mod setup;
mod state;
mod tailnet;
#[cfg(test)]
mod test_support;

pub(crate) use self::auth::is_loopback_authority;
pub(crate) use self::setup::{WebArgs, url_for};
pub(crate) use self::tailnet::tailnet;

use std::io::{IsTerminal, Read};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tiny_http::{Header, Method, Request, Response, Server};

use self::auth::{
    TOKEN_COOKIE, TOKEN_COOKIE_MAX_AGE, TOKEN_HEADER, bearer_ok, hosts_ok, origins_ok,
    random_token, same_token,
};
use self::bind::bind;
use self::detach::detach;
use self::routes::route;
use self::setup::{Serving, print_remote, setup};

use crate::home::{Home, contract_tilde};
use crate::run;

const INDEX_HTML: &str = include_str!("../web_dashboard.html");

/// Requests are handled on this many threads, so one slow one (say, an
/// action waiting for the lock) doesn't stall the dashboard's polling.
const WORKERS: usize = 4;

/// Largest request body read; the only body anyone sends is a short reason.
const MAX_BODY_BYTES: u64 = 16 * 1024;

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

fn json_reply(status: u16, body: String) -> Reply {
    reply(status, "application/json", body)
}

pub fn serve(home: &Home, args: WebArgs) -> Result<()> {
    home.ensure_initialized()?;
    let serving = setup(home, &args)?;
    if args.detach {
        return detach(home, false, args, &serving);
    }
    let (server, serving) = bind(home, &serving, args.replace)?;
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
    let (server, serving) = bind(home, &serving, args.replace)?;
    let _runner = if home.runner_alive() {
        println!("The runner is already running.");
        None
    } else {
        Some(spawn_runner(home)?)
    };
    run_server(home, server, &serving, &args)
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
                    handle(ctx, request, scope);
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

fn handle<'scope, 'env>(
    ctx: &'env Ctx<'env>,
    mut request: Request,
    scope: &'scope std::thread::Scope<'scope, 'env>,
) {
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
    } else if let Some(kind) = (!is_post).then(|| events::kind_of(&url)).flatten() {
        // A stream holds its thread for as long as the page is open, so it
        // gets one of its own instead of a worker.
        match events::claim_slot() {
            Some(slot) => {
                scope.spawn(move || events::serve(ctx, request, slot, kind));
                return;
            }
            None => reply(503, "text/plain", "too many open streams"),
        }
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
