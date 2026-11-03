//! `furmi update`: install a release over this installation.
//!
//! The other half of [`update_check`](crate::update_check), which reads the
//! same install receipt on every command to say a newer release exists. Both
//! only work for installs the release installer made: it is what writes the
//! receipt, and the receipt is what says where the install is and which version
//! is in it. A `cargo install` or a from-source build has nothing to compare
//! and nothing to replace, so those get told how to upgrade themselves instead
//! of a receipt error.

use anyhow::Result;
use axoupdater::{AxoUpdater, UpdateRequest};

use crate::home::Home;
use crate::version::{running, stale_summary};

/// The name the release installer files its receipt under, and what the
/// release assets are called.
const APP_NAME: &str = "furmiculi";
/// The version of the binary doing the updating: what `furmi --version` says.
const CURRENT: &str = crate::version::CURRENT;
const RELEASES: &str = "https://github.com/giannileggio/furmiculi/releases";

/// What the user asked `furmi update` to do.
pub struct UpdateArgs {
    /// Only say whether a newer release exists; install nothing.
    pub check: bool,
    /// Install this version instead of the latest, e.g. `0.4.2`. Explicit, so
    /// a downgrade works too.
    pub to: Option<String>,
    /// Run `furmi restart` (with the new binary) once the install is done.
    pub restart: bool,
}

/// A token for the GitHub API, when there is one to be had.
///
/// The releases are public, so this is optional: it lifts GitHub's anonymous
/// rate limit, and it is what makes an update work from a machine that can't
/// reach the API signed out. `GITHUB_TOKEN` and `GH_TOKEN` first (a service,
/// a unit, a shell that exports one), then whatever the `gh` CLI is signed in
/// as.
pub(crate) fn github_token() -> Option<String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!token.is_empty()).then_some(token)
}

/// The updater, signed in when a token is around.
pub(crate) fn updater() -> AxoUpdater {
    let mut updater = AxoUpdater::new_for(APP_NAME);
    if let Some(token) = github_token() {
        updater.set_github_token(&token);
    }
    updater
}

/// What to add to an update failure so it can be acted on.
const NEEDS_TOKEN: &str = "GitHub refused the request: set GITHUB_TOKEN (or GH_TOKEN), or \
     sign in with `gh auth login`. An update signed out is rate-limited hard.";
const NO_RELEASE: &str = "No release carries this app's installer yet: the newest one may \
     predate the rename, or none has been cut since. See {RELEASES}.";

/// The advice for a failed update, if there is any to give.
pub(crate) fn hint(error: &axoupdater::AxoupdateError, signed_in: bool) -> Option<&'static str> {
    match error {
        axoupdater::AxoupdateError::NoStableReleases { .. }
        | axoupdater::AxoupdateError::NoInstallerForPackage {} => Some(NO_RELEASE),
        // GitHub answers an anonymous request it won't serve with a bare 401,
        // 403 or 404 (a rate limit, or a repository it won't show) rather than
        // anything that names the problem.
        _ if !signed_in && mentions_status(&error.to_string()) => Some(NEEDS_TOKEN),
        _ => None,
    }
}

fn mentions_status(text: &str) -> bool {
    ["401", "403", "404"]
        .iter()
        .any(|status| text.contains(status))
}

/// The error as the user sees it: the failure, then the advice.
pub(crate) fn explain(error: axoupdater::AxoupdateError, signed_in: bool) -> anyhow::Error {
    match hint(&error, signed_in) {
        Some(hint) => anyhow::anyhow!("{error}\n\n{}", hint.replace("{RELEASES}", RELEASES)),
        None => error.into(),
    }
}

pub fn update(home: &Home, args: UpdateArgs) -> Result<()> {
    let mut updater = match receipt_loaded() {
        Ok(updater) => updater,
        Err(axoupdater::AxoupdateError::NoReceipt { .. }) => anyhow::bail!(
            "this furmi can't update itself: it wasn't installed by the release installer, so \
             there is no install receipt to check or replace. Upgrade it the way it was \
             installed (`cargo install --locked furmiculi`, mise, your package manager), or get \
             the release from {RELEASES}"
        ),
        Err(e) => return Err(e.into()),
    };

    if let Some(version) = &args.to {
        updater
            .configure_version_specifier(UpdateRequest::SpecificVersion(version.clone()))
            // Asked for by name: install it even when it is older than what is
            // already here (a downgrade is a real answer to a bad release).
            .always_update(true);
    }

    if args.check {
        return check(&mut updater);
    }
    let installed = install(&mut updater)?;
    after_install(home, &installed, args.restart)
}

/// An updater that knows its install, or the reason it doesn't.
///
/// axoupdater's error type trips clippy::result_large_err, and it allows the
/// same lint internally for the same reason.
#[allow(clippy::result_large_err)]
fn receipt_loaded() -> axoupdater::AxoupdateResult<AxoUpdater> {
    let mut updater = updater();
    updater.load_receipt()?;
    Ok(updater)
}

/// "Is there a newer release?" — what `--check` answers, without installing
/// anything.
fn check(updater: &mut AxoUpdater) -> Result<()> {
    let signed_in = github_token().is_some();
    if !updater
        .is_update_needed_sync()
        .map_err(|e| explain(e, signed_in))?
    {
        println!("furmiculi {CURRENT} is the latest release.");
        return Ok(());
    }
    // Which release it is would take a second request (`query_new_version` is
    // async only), and the answer to "is there one?" is what was asked.
    println!(
        "A newer furmiculi release is available (you have {CURRENT}). \
         Run `furmi update` to install it, or see {RELEASES}."
    );
    Ok(())
}

/// Downloads and runs the installer from the release, which replaces this
/// binary where it is. The installer's own progress is left on screen: a
/// download with nothing to show for it looks like nothing happening.
fn install(updater: &mut AxoUpdater) -> Result<String> {
    let signed_in = github_token().is_some();
    Ok(
        match updater.run_sync().map_err(|e| explain(e, signed_in))? {
            Some(result) => {
                println!(
                    "Updated furmiculi {} to {}.",
                    result
                        .old_version
                        .map_or_else(|| CURRENT.to_string(), |v| v.to_string()),
                    result.new_version
                );
                result.new_version.to_string()
            }
            None => {
                println!("furmiculi {CURRENT} is the latest release.");
                CURRENT.to_string()
            }
        },
    )
}

/// The line `furmi update` ends with when something is still on the old release.
fn warning(installed: &str, behind: &str) -> String {
    format!(
        "installed {installed}; {behind}, run `furmi restart` (or `furmi update --restart` next time)"
    )
}

/// What a replaced binary doesn't change: a dashboard or runner already
/// running keeps executing the old one. Says so, and restarts them only when
/// asked: an update can run inside a task or a script, whose own runner a
/// restart would stop.
fn after_install(home: &Home, installed: &str, restart: bool) -> Result<()> {
    let behind = stale_summary(&running(home), installed);
    let Some(behind) = behind else {
        return Ok(());
    };
    if !restart {
        println!("{}", warning(installed, &behind));
        return Ok(());
    }
    println!("Installed {installed}; {behind}. Restarting them.");
    // The new binary does the restarting: this process is still the old one.
    let status = std::process::Command::new(crate::exe::path()?)
        .arg("--home")
        .arg(home.root())
        .arg("restart")
        .status()?;
    if !status.success() {
        anyhow::bail!("`furmi restart` failed ({status}); run it again by hand");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axoupdater::AxoupdateError;

    fn no_release() -> AxoupdateError {
        AxoupdateError::NoStableReleases {
            app_name: APP_NAME.to_string(),
        }
    }

    #[test]
    fn the_update_ends_by_naming_what_still_runs_the_old_release() {
        assert_eq!(
            warning("0.5.0", "dashboard pid 7 and runner pid 9 still run 0.4.0"),
            "installed 0.5.0; dashboard pid 7 and runner pid 9 still run 0.4.0, \
             run `furmi restart` (or `furmi update --restart` next time)"
        );
    }

    #[test]
    fn a_refused_request_without_a_token_says_what_to_set() {
        // What GitHub answers when it won't serve an anonymous request (a rate
        // limit, or a repository it hides).
        let text = "HTTP status client error (404 Not Found) for url \
                    (https://api.github.com/repos/giannileggio/furmiculi/releases)";
        assert!(mentions_status(text));
        assert_eq!(
            hint(&no_release(), true),
            Some(NO_RELEASE),
            "a 404 in a status, not here"
        );
        assert_eq!(
            hint(
                &AxoupdateError::NoReceipt {
                    app_name: APP_NAME.to_string()
                },
                false
            ),
            None,
            "not a status failure: nothing to say about tokens"
        );
    }

    #[test]
    fn a_release_without_this_apps_installer_says_so() {
        let hint = hint(&no_release(), false).expect("advice");
        assert!(hint.contains("installer"), "{hint}");
        assert!(
            hint.contains("{RELEASES}"),
            "the placeholder is filled in later"
        );
        let shown = explain(no_release(), false).to_string();
        assert!(shown.contains(RELEASES), "{shown}");
        assert!(
            shown.contains("no stable releases"),
            "the failure is still there: {shown}"
        );
    }

    #[test]
    fn anything_else_is_passed_through_untouched() {
        let error = AxoupdateError::ReceiptLoadFailed {
            app_name: APP_NAME.to_string(),
        };
        let shown = explain(error, true).to_string();
        assert_eq!(
            shown,
            "Unable to read installation information for app furmiculi."
        );
    }

    #[test]
    fn only_http_status_codes_count_as_a_closed_door() {
        assert!(mentions_status(
            "HTTP status client error (401 Unauthorized)"
        ));
        assert!(mentions_status("HTTP status client error (403 Forbidden)"));
        assert!(!mentions_status("no stable releases available"));
    }
}
