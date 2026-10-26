//! The host names a remote dashboard should accept, suggested from this box's
//! tailnet: `tailscale status --json`, and why there is nothing to suggest
//! when it can't be read.

use std::process::Command;
use std::time::Duration;

/// This box as its tailnet sees it: the MagicDNS name and the tailscale IPv4
/// a browser might use instead. None without a working, logged-in tailscale.
pub(crate) struct Tailnet {
    pub(crate) name: String,
    ip: Option<String>,
}

impl Tailnet {
    /// Names the dashboard should accept, advertised in this order.
    pub(super) fn hosts(&self) -> Vec<String> {
        std::iter::once(self.name.clone())
            .chain(self.ip.clone())
            .collect()
    }
}

/// [`tailnet`], and why there's nothing to offer when there isn't.
pub(crate) enum TailnetLookup {
    Found(Tailnet),
    /// No name: `tailscale` isn't installed, or its daemon isn't reachable
    /// from here — on a container the daemon usually runs on the host, so
    /// this box can't ask it what it's called.
    Unavailable(String),
}

impl TailnetLookup {
    pub(crate) fn found(self) -> Option<Tailnet> {
        match self {
            Self::Found(tailnet) => Some(tailnet),
            Self::Unavailable(_) => None,
        }
    }
}

pub(crate) fn tailnet() -> TailnetLookup {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
