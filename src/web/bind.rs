//! Taking the address to serve: binding it, reporting the real port behind a
//! `:0`, and taking the address over from this home's own dashboard when it
//! already holds it.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::Result;
use tiny_http::Server;

use super::auth::split_authority;
use super::setup::{Serving, ask_yes_no};

use crate::home::Home;

/// The address to report for a bound socket: `requested`, unless it asked for
/// a port the OS picked (`:0`), in which case it is whatever the socket says.
pub(super) fn real_addr(requested: &str, server: &Server) -> String {
    let port_is_placeholder = requested.rsplit_once(':').is_some_and(|(_, p)| p == "0");
    if !port_is_placeholder {
        return requested.to_string();
    }
    match server.server_addr().to_ip() {
        Some(bound) => bound.to_string(),
        // Not an IP socket (`ListenAddr` also covers unix sockets, which
        // `Server::http` never makes): nothing better to say than this.
        None => requested.to_string(),
    }
}

/// Binds the dashboard. When something already holds the address, and it's
/// this home's own dashboard, offer to stop it and take over (`--replace`, or
/// a yes at the prompt); everything else keeps the original error.
///
/// Returns what to serve it as: a `:0` port asks the OS for a free one, so
/// the address the caller asked for is only a placeholder and the real one
/// has to come back from the bound socket.
pub(super) fn bind(home: &Home, serving: &Serving, replace: bool) -> Result<(Server, Serving)> {
    let addr = &serving.addr;
    let busy = match Server::http(addr.as_str()) {
        Ok(server) => {
            let bound = serving.bound(&server);
            return Ok((server, bound));
        }
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
    let server = stop_dashboard(pid, addr)?.map_or_else(
        || {
            Err(busy_error(
                addr,
                &format!("the dashboard on it (pid {pid}) stopped, but the address is still busy"),
            ))
        },
        Ok,
    )?;
    let bound = serving.bound(&server);
    Ok((server, bound))
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
pub(super) fn same_port(a: &str, b: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_port_zero_bind_reports_the_port_the_os_gave() {
        // The e2e tests start one dashboard per test with `--port 0`, so the
        // kernel picks a free port for each and two can never collide on one
        // they merely guessed was free. That only works if the URL they read
        // back carries the real port.
        let server = Server::http("127.0.0.1:0").unwrap();
        let bound = real_addr("127.0.0.1:0", &server);
        let port: u16 = bound.trim_start_matches("127.0.0.1:").parse().unwrap();
        assert_ne!(port, 0);
        assert_eq!(bound, server.server_addr().to_ip().unwrap().to_string());

        // A port that was actually asked for is left exactly as asked, even
        // when it is one the server would have chosen anyway: `--listen`
        // names the host a browser uses, which is not always the IP bound.
        let server = Server::http("127.0.0.1:0").unwrap();
        assert_eq!(real_addr("box.tailnet:7433", &server), "box.tailnet:7433");
        assert_eq!(real_addr("0.0.0.0:7433", &server), "0.0.0.0:7433");
    }
}
