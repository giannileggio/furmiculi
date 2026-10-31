//! Who may talk to the dashboard: the bearer token a remote bind needs, the
//! `Host` allowlist, the `Origin` and per-run token a POST needs, and the
//! cookie that saves a remote browser from carrying the token in the URL.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use tiny_http::Method;

use super::Ctx;
use super::routes::query_param;

use crate::home::{contract_tilde, expand_tilde};

pub(super) const TOKEN_HEADER: &str = "X-Furmi-Token";

/// The cookie a remote bind hands the browser after a first load carrying
/// `?token=`, so a refresh (or a bookmark) doesn't ask for the token again.
pub(super) const TOKEN_COOKIE: &str = "furmi_token";

/// How long that cookie lives. Long enough that a bookmark still works next
/// month; it is re-set on every page load.
pub(super) const TOKEN_COOKIE_MAX_AGE: u64 = 30 * 24 * 60 * 60;

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

/// The bearer gate for a remote bind: `Authorization: Bearer <t>` on every
/// route; the dashboard page also gets in on a first load carrying the token
/// as `?token=` and, afterwards, on the cookie the server set for it (so a
/// refresh or a bookmark doesn't have to carry the token in the URL).
/// Nothing to check on a loopback bind.
pub(super) fn bearer_ok(
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
