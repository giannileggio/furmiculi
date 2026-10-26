//! Helpers shared by the web module's unit tests.

use std::path::PathBuf;

use super::setup::WebArgs;

/// A 0600 token file, the way `furmi web --setup` makes one.
pub(super) fn token_file(dir: &std::path::Path, token: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("web-token");
    std::fs::write(&path, format!("{token}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// A [`WebArgs`] with everything default but the address and hosts.
pub(super) fn args(listen: Option<&str>, hosts: &[&str]) -> WebArgs {
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
