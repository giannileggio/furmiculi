//! Which version each long-lived process is running, against the one installed.
//!
//! `furmi update` replaces the binary on disk, but a dashboard or runner that
//! was already running keeps executing the old one, and nothing used to say so.
//! The dashboard and the runner now write their version into their lock files;
//! this module reads those back and says which are behind.

use std::cmp::Ordering;

use crate::home::Home;

/// The version of this binary: the installed one, as far as a `furmi` command
/// run from a shell is concerned.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Compares two `major.minor.patch[-pre]` versions. `None` when either isn't
/// numeric, so an odd version string is never called older or newer.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    fn parts(v: &str) -> Option<(Vec<u64>, bool)> {
        let v = v.trim().trim_start_matches('v');
        let v = v.split('+').next()?;
        let (core, pre) = match v.split_once('-') {
            Some((core, _)) => (core, true),
            None => (v, false),
        };
        let nums = core
            .split('.')
            .map(|n| n.parse().ok())
            .collect::<Option<Vec<u64>>>()?;
        Some((nums, pre))
    }
    let (a, a_pre) = parts(a)?;
    let (b, b_pre) = parts(b)?;
    // 1.0 == 1.0.0; a pre-release sorts before its release.
    let len = a.len().max(b.len());
    for i in 0..len {
        let ord = a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0));
        if ord != Ordering::Equal {
            return Some(ord);
        }
    }
    Some(b_pre.cmp(&a_pre))
}

/// Whether a process reporting `running` (None: its lock file predates
/// versions, so it is older than any binary that writes one) needs a restart
/// to be the `installed` version.
pub fn is_stale(running: Option<&str>, installed: &str) -> bool {
    match running {
        None => true,
        Some(v) => v != installed,
    }
}

/// "0.4.0", or what to say instead when the process didn't record one.
pub fn label(version: Option<&str>) -> String {
    version.map_or_else(|| "an older version".to_string(), str::to_string)
}

/// One long-lived process and the version it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub name: &'static str,
    pub pid: u32,
    pub version: Option<String>,
}

/// The dashboard and the runner serving this home right now.
pub fn running(home: &Home) -> Vec<Proc> {
    let mut procs = Vec::new();
    if let Some(d) = home.dashboard_info() {
        procs.push(Proc {
            name: "dashboard",
            pid: d.pid,
            version: d.version,
        });
    }
    if let Some(pid) = home.runner_pid() {
        procs.push(Proc {
            name: "runner",
            pid,
            version: home.runner_version(),
        });
    }
    procs
}

/// "dashboard pid 7 and runner pid 9 still run 0.4.0", for the processes that
/// aren't `installed`; None when none is behind.
pub fn stale_summary(procs: &[Proc], installed: &str) -> Option<String> {
    let stale: Vec<&Proc> = procs
        .iter()
        .filter(|p| is_stale(p.version.as_deref(), installed))
        .collect();
    if stale.is_empty() {
        return None;
    }
    // Processes on the same version share a clause.
    let mut groups: Vec<(String, Vec<&Proc>)> = Vec::new();
    for p in stale {
        let l = label(p.version.as_deref());
        match groups.iter_mut().find(|(g, _)| *g == l) {
            Some((_, members)) => members.push(p),
            None => groups.push((l, vec![p])),
        }
    }
    let clauses: Vec<String> = groups
        .iter()
        .map(|(version, members)| {
            let names: Vec<String> = members
                .iter()
                .map(|p| format!("{} pid {}", p.name, p.pid))
                .collect();
            format!(
                "{} still run{} {version}",
                names.join(" and "),
                if members.len() == 1 { "s" } else { "" }
            )
        })
        .collect();
    Some(clauses.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(name: &'static str, pid: u32, version: Option<&str>) -> Proc {
        Proc {
            name,
            pid,
            version: version.map(String::from),
        }
    }

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(compare("0.4.0", "0.5.0"), Some(Ordering::Less));
        assert_eq!(compare("0.10.0", "0.9.9"), Some(Ordering::Greater));
        assert_eq!(compare("1.0", "1.0.0"), Some(Ordering::Equal));
        assert_eq!(compare("0.5.0-rc.1", "0.5.0"), Some(Ordering::Less));
        assert_eq!(compare("nightly", "0.5.0"), None);
    }

    #[test]
    fn a_process_without_a_recorded_version_is_stale() {
        assert!(is_stale(None, "0.5.0"));
        assert!(is_stale(Some("0.4.0"), "0.5.0"));
        assert!(!is_stale(Some("0.5.0"), "0.5.0"));
    }

    #[test]
    fn the_summary_names_who_is_behind() {
        let both = [
            proc("dashboard", 7, Some("0.4.0")),
            proc("runner", 9, Some("0.4.0")),
        ];
        assert_eq!(
            stale_summary(&both, "0.5.0").unwrap(),
            "dashboard pid 7 and runner pid 9 still run 0.4.0"
        );
        let mixed = [proc("dashboard", 7, Some("0.5.0")), proc("runner", 9, None)];
        assert_eq!(
            stale_summary(&mixed, "0.5.0").unwrap(),
            "runner pid 9 still runs an older version"
        );
        assert_eq!(
            stale_summary(&[proc("runner", 9, Some("0.5.0"))], "0.5.0"),
            None
        );
        assert_eq!(stale_summary(&[], "0.5.0"), None);
    }
}
