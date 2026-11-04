//! Who may talk to the dashboard: the bearer token a remote bind needs, the
//! `Host` allowlist, the `Origin` and per-run token a POST needs, and the
//! short-lived session cookie that saves a remote browser from carrying the
//! token in the URL.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tiny_http::Method;

use super::Ctx;
use super::routes::{is_page, query_param};

use crate::home::{contract_tilde, expand_tilde};

pub(super) const TOKEN_HEADER: &str = "X-Furmi-Token";

/// The cookie a remote bind hands the browser in exchange for a first load
/// carrying `?token=`. It holds a random session id, never the token itself,
/// so a copy of it (history sync, a screen share) is worth minutes, not a
/// month, and says nothing about the master token.
pub(super) const SESSION_COOKIE: &str = "furmi_session";

/// The cookie earlier versions set, holding the master token for 30 days. It
/// is no longer read; page loads tell the browser to throw it away.
pub(super) const LEGACY_TOKEN_COOKIE: &str = "furmi_token";

/// How long a session lives past its last request. Every request through the
/// server (and the page's keepalive) pushes it out again, in the browser's
/// cookie and in the server's table.
pub(super) const SESSION_TTL: Duration = Duration::from_secs(15 * 60);

/// Sessions kept at once; the soonest to expire makes room for a new one.
const MAX_SESSIONS: usize = 64;

struct Session {
    expires: Instant,
    /// What the page sends as `X-Furmi-Token` (and as its bearer): the page
    /// is served this instead of the master token, so it dies with the session.
    page_token: String,
}

/// The sessions a remote bind has handed out. In memory only: restarting the
/// server (which is also how a changed token file is picked up) drops them all.
pub(super) struct Sessions {
    ttl: Duration,
    map: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub(super) fn new(ttl: Duration) -> Self {
        Sessions {
            ttl,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// A fresh session: its id (for the cookie) and its page token.
    pub(super) fn open(&self, now: Instant) -> Result<(String, String)> {
        let (id, page_token) = (random_token()?, random_token()?);
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, s| s.expires > now);
        while map.len() >= MAX_SESSIONS {
            let Some(oldest) = map
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            map.remove(&oldest);
        }
        map.insert(
            id.clone(),
            Session {
                expires: now + self.ttl,
                page_token: page_token.clone(),
            },
        );
        Ok((id, page_token))
    }

    /// If `id` is a live session: pushes its expiry out and returns its page token.
    pub(super) fn touch(&self, id: &str, now: Instant) -> Option<String> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let session = map.get_mut(id)?;
        if session.expires <= now {
            map.remove(id);
            return None;
        }
        session.expires = now + self.ttl;
        Some(session.page_token.clone())
    }
}

/// How a request got past the gate.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Access {
    /// Loopback, or the master token as a bearer.
    Open,
    /// A page GET carrying `?token=`: to be traded for a session.
    Exchange,
    /// A live session cookie; the session's page token comes along.
    Session {
        id: String,
        page_token: String,
    },
    Denied,
}

/// Same string, without telling an attacker where it first differs: all
/// bytes are folded into one number in the same time either way.
pub(super) fn same_token(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).unwrap_or(&0) ^ b.get(i).unwrap_or(&0);
    }
    diff == 0
}

/// 128 random bits from the OS, hex-encoded.
pub(super) fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom for the session token")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Reads the token file: one line, trimmed. It should be 0600; a group- or
/// world-readable one is refused, the way a shared secret shouldn't sit.
pub(crate) fn read_token_file(path: &std::path::Path) -> Result<String> {
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

/// Creates the token file (0600, 128 random bits) if it isn't there yet. An
/// existing one is left alone, so cookies already handed out keep working.
pub(super) fn ensure_token_file(path: &std::path::Path) -> Result<()> {
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

/// The allowlist entry that stands for "any `Host`": for a wide bind whose
/// name browsers will use isn't knowable (a laptop on several networks, a box
/// reached under more than one name). The bearer token still gates every
/// request; the allowlist just stops being the second lock.
pub(super) const ANY_HOST: &str = "*";

pub(super) fn is_any_host(entry: &str) -> bool {
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
pub(super) fn split_authority(authority: &str) -> (&str, Option<&str>) {
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
pub(super) fn hosts_ok(ctx: &Ctx, host: &str) -> bool {
    if ctx.remote {
        host_allowed(host, &ctx.allowed_hosts)
    } else {
        is_loopback_authority(host)
    }
}

/// Which `Origin` headers a POST accepts: loopback for either mode; a remote
/// bind additionally accepts an origin whose host is on its allowlist (the
/// dashboard page itself, loaded from `http://tools.example:7433`).
pub(super) fn origins_ok(ctx: &Ctx, origin: &str) -> bool {
    is_loopback_origin(origin) || (ctx.remote && origin_allowed(origin, &ctx.allowed_hosts))
}

/// The gate for a remote bind: the master token as `Authorization: Bearer <t>`
/// on any route; a page GET carrying it as `?token=` (to be exchanged for a
/// session, see [`Access::Exchange`]); or a live session cookie. Nothing to
/// check on a loopback bind.
pub(super) fn authenticate(
    ctx: &Ctx,
    sessions: &Sessions,
    method: &Method,
    url: &str,
    authorization: Option<&str>,
    cookie: Option<&str>,
    now: Instant,
) -> Access {
    if !ctx.remote {
        return Access::Open;
    }
    if let Some(supplied) = authorization.and_then(|a| a.strip_prefix("Bearer "))
        && same_token(supplied, &ctx.token)
    {
        return Access::Open;
    }
    let path = url.split('?').next().unwrap_or("");
    if *method == Method::Get
        && is_page(&segments(path))
        && query_param(url, "token").is_some_and(|t| same_token(t, &ctx.token))
    {
        return Access::Exchange;
    }
    // (The page's own bearer is its session's page token, which only means
    // something next to the cookie, so a bearer that isn't the master falls
    // through to the cookie.)
    cookie
        .and_then(|c| cookie_value(c, SESSION_COOKIE))
        .and_then(|id| {
            sessions.touch(id, now).map(|page_token| Access::Session {
                id: id.to_string(),
                page_token,
            })
        })
        .unwrap_or(Access::Denied)
}

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// Where an exchanged page load goes next: the same page without `token`.
/// Rebuilt from the path's segments, so it is always a plain local path.
pub(super) fn stripped_location(url: &str) -> String {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let mut out = format!("/{}", segments(path).join("/"));
    let rest: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && p.split('=').next() != Some("token"))
        .collect();
    if !rest.is_empty() {
        out.push('?');
        out.push_str(&rest.join("&"));
    }
    out
}

/// The `Set-Cookie` value for a session; `Secure` when the request came in
/// over TLS (a proxy says so with `X-Forwarded-Proto: https`; the server
/// itself speaks plain HTTP, so a LAN bind without a proxy can't have it).
pub(super) fn session_cookie(id: &str, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={id}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        SESSION_TTL.as_secs(),
        if secure { "; Secure" } else { "" }
    )
}

/// Tells the browser to forget the 30-day cookie older versions set.
pub(super) fn legacy_cookie_expiry() -> String {
    format!("{LEGACY_TOKEN_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// Was the request made over TLS, going by the proxy's `X-Forwarded-Proto`?
pub(super) fn forwarded_https(proto: Option<&str>) -> bool {
    proto.is_some_and(|p| {
        p.split(',')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("https")
    })
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_session_cookie_is_found_among_the_others() {
        assert_eq!(
            cookie_value("furmi_session=abc", SESSION_COOKIE),
            Some("abc")
        );
        assert_eq!(
            cookie_value("other=1; furmi_session=abc; third=2", SESSION_COOKIE),
            Some("abc")
        );
        assert_eq!(
            cookie_value(" furmi_session = abc ", SESSION_COOKIE),
            Some("abc")
        );
        assert_eq!(cookie_value("other=1", SESSION_COOKIE), None);
        assert_eq!(cookie_value("furmi_session=", SESSION_COOKIE), None);
        assert_eq!(cookie_value("furmi_session", SESSION_COOKIE), None);
        assert_eq!(cookie_value("furmi_sessions=abc", SESSION_COOKIE), None);
        assert_eq!(cookie_value("", SESSION_COOKIE), None);
    }

    fn remote_ctx<'a>(home: &'a crate::home::Home, token: &str) -> Ctx<'a> {
        Ctx {
            home,
            token: token.to_string(),
            index: String::new(),
            cwd_repo: None,
            remote: true,
            allowed_hosts: vec!["*".to_string()],
            peer: false,
        }
    }

    fn home() -> (tempfile::TempDir, crate::home::Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = crate::home::Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        (dir, home)
    }

    #[test]
    fn the_token_is_traded_for_a_session_that_expires() {
        let (_dir, home) = home();
        let ctx = remote_ctx(&home, "master");
        let sessions = Sessions::new(SESSION_TTL);
        let now = Instant::now();
        let auth = |url: &str, cookie: Option<&str>, at: Instant| {
            authenticate(&ctx, &sessions, &Method::Get, url, None, cookie, at)
        };

        // No credentials, a wrong token, or the token on a non-page route.
        assert_eq!(auth("/", None, now), Access::Denied);
        assert_eq!(auth("/?token=nope", None, now), Access::Denied);
        assert_eq!(auth("/api/state?token=master", None, now), Access::Denied);
        // The right one on a page is exchanged...
        assert_eq!(auth("/?token=master", None, now), Access::Exchange);
        // ...for a session, which then carries the request.
        let (id, page_token) = sessions.open(now).unwrap();
        assert_ne!(id, "master");
        let cookie = format!("{SESSION_COOKIE}={id}");
        let live = Access::Session {
            id: id.clone(),
            page_token,
        };
        assert_eq!(auth("/api/state", Some(&cookie), now), live);
        // The old cookie, holding the master token, no longer opens anything.
        assert_eq!(auth("/", Some("furmi_token=master"), now), Access::Denied);
        // Each request pushes the expiry out; idle past the TTL ends it.
        let step = SESSION_TTL - Duration::from_secs(1);
        assert_eq!(auth("/", Some(&cookie), now + step), live);
        assert_eq!(auth("/", Some(&cookie), now + step * 2), live);
        let idle = now + step * 2 + SESSION_TTL;
        assert_eq!(auth("/", Some(&cookie), idle), Access::Denied);
        // Gone for good, not just late.
        assert_eq!(auth("/", Some(&cookie), now), Access::Denied);
    }

    #[test]
    fn the_bearer_and_loopback_still_open_everything() {
        let (_dir, home) = home();
        let ctx = remote_ctx(&home, "master");
        let sessions = Sessions::new(SESSION_TTL);
        let now = Instant::now();
        let get = |authz| {
            authenticate(
                &ctx,
                &sessions,
                &Method::Get,
                "/api/state",
                authz,
                None,
                now,
            )
        };
        assert_eq!(get(Some("Bearer master")), Access::Open);
        assert_eq!(get(Some("Bearer wrong")), Access::Denied);
        assert_eq!(get(Some("master")), Access::Denied);

        let loopback = Ctx {
            remote: false,
            ..remote_ctx(&home, "x")
        };
        assert_eq!(
            authenticate(
                &loopback,
                &sessions,
                &Method::Get,
                "/api/state",
                None,
                None,
                now
            ),
            Access::Open
        );
    }

    #[test]
    fn a_restart_with_a_new_token_drops_every_session() {
        let (_dir, home) = home();
        let now = Instant::now();
        let old = (remote_ctx(&home, "old"), Sessions::new(SESSION_TTL));
        let (id, _) = old.1.open(now).unwrap();
        let cookie = format!("{SESSION_COOKIE}={id}");
        // The rotated token is a new server with an empty table.
        let new = (remote_ctx(&home, "new"), Sessions::new(SESSION_TTL));
        let get = |(ctx, sessions): &(Ctx, Sessions)| {
            authenticate(ctx, sessions, &Method::Get, "/", None, Some(&cookie), now)
        };
        assert_ne!(get(&old), Access::Denied);
        assert_eq!(get(&new), Access::Denied);
        assert_eq!(
            authenticate(&new.0, &new.1, &Method::Get, "/?token=old", None, None, now),
            Access::Denied
        );
    }

    #[test]
    fn sessions_are_capped() {
        let sessions = Sessions::new(SESSION_TTL);
        let now = Instant::now();
        let (first, _) = sessions.open(now).unwrap();
        for i in 1..=MAX_SESSIONS {
            sessions.open(now + Duration::from_secs(i as u64)).unwrap();
        }
        assert!(sessions.touch(&first, now).is_none());
    }

    #[test]
    fn the_exchange_strips_the_token_and_stays_local() {
        assert_eq!(stripped_location("/?token=abc"), "/");
        assert_eq!(
            stripped_location("/tasks/t1?token=abc&x=1"),
            "/tasks/t1?x=1"
        );
        assert_eq!(stripped_location("/?x=1&token=abc&y=2"), "/?x=1&y=2");
        // Never protocol-relative, whatever the path looks like.
        assert_eq!(
            stripped_location("//evil.example/?token=abc"),
            "/evil.example"
        );
        assert_eq!(stripped_location("///?token=abc"), "/");
    }

    #[test]
    fn the_cookie_is_http_only_strict_and_secure_only_over_tls() {
        let plain = session_cookie("id", false);
        assert_eq!(
            plain,
            "furmi_session=id; Path=/; HttpOnly; SameSite=Strict; Max-Age=900"
        );
        assert!(session_cookie("id", true).ends_with("; Secure"));
        assert!(forwarded_https(Some("https")));
        assert!(forwarded_https(Some("HTTPS, http")));
        assert!(!forwarded_https(Some("http")));
        assert!(!forwarded_https(None));
        assert!(legacy_cookie_expiry().contains("Max-Age=0"));
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
}
