//! `furmi web`'s own arguments, and how the dashboard decides where to listen:
//! the flags, then `[web]` in config.toml, then the once-only `--setup`
//! questions, whose answers are saved for later runs.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::Result;
use tiny_http::Server;

use super::auth::{
    ANY_HOST, ensure_token_file, is_any_host, is_loopback_authority, read_token_file,
    split_authority,
};
use super::bind::real_addr;
use super::tailnet::{TailnetLookup, tailnet};

use crate::config::{Config, Web, set_web_config};
use crate::home::{Home, contract_tilde, expand_tilde};

#[derive(clap::Args)]
pub(crate) struct WebArgs {
    /// Port to listen on (ignored when --listen names the port itself).
    #[arg(long, default_value_t = 7433)]
    pub(super) port: u16,
    /// Address to bind, e.g. `0.0.0.0:7433` to serve other machines (a
    /// tailnet). Default, and unless the address is non-loopback, loopback
    /// checks apply unchanged.
    #[arg(long)]
    pub(super) listen: Option<String>,
    /// File with the bearer token for `--listen` (created/made 0600 by the
    /// operator; read once at startup, kept out of argv).
    #[arg(long)]
    pub(super) token_file: Option<PathBuf>,
    /// Bearer token for `--listen`, right on the command line (--token-file
    /// is preferred: command lines end up in shell history and ps output).
    #[arg(long)]
    pub(super) token: Option<String>,
    /// Host an HTTP `Host` header must carry exactly to be served, for
    /// non-loopback `--listen` binds. Repeatable. `host`, `host:port`, or `*`
    /// to accept any host (the token still gates every request).
    #[arg(long = "allowed-host")]
    pub(super) allowed_hosts: Vec<String>,
    /// Ask where to serve from (your tailnet? which host name?) and save the
    /// answers in config.toml, then serve. Also runs on its own the first
    /// time a non-loopback `--listen` is given without a token and a host.
    #[arg(long)]
    pub(super) setup: bool,
    /// Stop a dashboard already serving this address in this home and take
    /// it over, instead of failing. When the address is busy and furmi can
    /// tell whose dashboard it is, running in a terminal asks instead.
    #[arg(long)]
    pub(super) replace: bool,
    /// Don't open the dashboard in a browser.
    #[arg(long)]
    pub(super) no_open: bool,
    /// Serve in the background and return: the dashboard (and the runner, for
    /// `furmi start`) keeps going after this shell closes, logging to web.log
    /// in the data folder. `furmi stop` stops it.
    #[arg(long, short = 'd')]
    pub(super) detach: bool,
}

/// How the dashboard listens. `remote` is a wide, headless-style bind:
/// requests then have to prove themselves with a bearer token and an
/// allowlisted `Host`, on top of the loopback-era checks.
#[derive(Debug, Clone)]
pub(super) struct Serving {
    pub(super) addr: String,
    pub(super) remote: bool,
    /// Shared secret for a remote bind; None on the loopback default, where
    /// the per-run token in the page is the only secret.
    pub(super) bearer: Option<String>,
    /// Exact `Host` allowlist for a remote bind.
    pub(super) allowed_hosts: Vec<String>,
    /// Where the bearer came from (`~/.furmiculi/web-token`, `--token`), for
    /// the line that says how to get it.
    token_from: Option<String>,
}

impl Serving {
    /// This serving with the address the socket really got. A `:0` port (what
    /// a caller that doesn't care which port it lands on asks for, so two of
    /// them can run side by side) is only a request for a free port: the
    /// placeholder has to be replaced by the real one before it is printed,
    /// recorded in the lock file, or handed to a browser.
    pub(super) fn bound(&self, server: &Server) -> Serving {
        Serving {
            addr: real_addr(&self.addr, server),
            ..self.clone()
        }
    }

    /// The URL a browser on another machine opens, for the first allowlisted
    /// host. `with_token` puts the secret in the query string: only do that
    /// on a terminal, never into a log file or the journal.
    pub(super) fn open_url(&self, token: &str, with_token: bool) -> Option<String> {
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
pub(super) fn setup(home: &Home, args: &WebArgs) -> Result<Serving> {
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

pub(super) fn ask_yes_no(prompt: &str, default: bool) -> Result<bool> {
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

/// Where a browser on another machine reaches a wide bind, and with what: the
/// token goes in the URL only on a terminal, so a unit's journal or a log file
/// never keeps the secret around.
pub(super) fn print_remote(serving: &Serving, token: &str, tty: bool) {
    if let Some(open) = serving.open_url(token, tty) {
        println!("Serving other machines. Open this once, and the page keeps a cookie:");
        println!("  {open}");
    }
    println!(
        "  Host: {}; token in {}",
        hosts_summary(&serving.allowed_hosts),
        serving.token_from.as_deref().unwrap_or("--token")
    );
    if serving.allowed_hosts.iter().any(|h| is_any_host(h)) {
        println!(
            "  Warning: any Host is accepted, so the token is the only gate, over plain HTTP.\n  \
             Keep this behind tailscale/an SSH tunnel, or pin a name with --allowed-host."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::test_support::{args, token_file};

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
}
