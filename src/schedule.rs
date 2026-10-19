use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local};
use croner::Cron;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::frontmatter;
use crate::fsutil;
use crate::spec::{SPEC_KEYS, TaskSpec, validate_id};
use crate::task::prompt_problems;

/// A recurring task template: `schedules/<id>.md`. The body is the prompt
/// given to every task it enqueues.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    /// Standard 5-field cron expression, evaluated in local time.
    pub cron: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Enqueue even if the previous run is still pending or running.
    /// Off by default: a slow task isn't joined by a pile of copies.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_overlap: bool,
    #[serde(flatten)]
    pub spec: TaskSpec,

    // Written by furmiculi only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_enqueued_at: Option<DateTime<FixedOffset>>,

    #[serde(skip)]
    pub prompt: String,
}

const SCHEDULE_KEYS: &[&str] = &["id", "cron", "enabled", "allow_overlap", "last_enqueued_at"];

fn yes() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Schedule {
    pub fn parse(content: &str) -> Result<Self> {
        let allowed: Vec<&str> = SCHEDULE_KEYS.iter().chain(SPEC_KEYS).copied().collect();
        let (mut schedule, body): (Schedule, String) = frontmatter::parse(content, &allowed)?;
        schedule.prompt = body;
        Ok(schedule)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = frontmatter::render(self, &self.prompt)?;
        fsutil::write_atomic(path, text)
    }

    pub fn cron(&self) -> Result<Cron> {
        self.cron
            .parse::<Cron>()
            .map_err(|e| anyhow::anyhow!("invalid cron `{}`: {e}", self.cron))
    }

    /// Next firing strictly after `after`.
    pub fn next_after(&self, after: DateTime<Local>) -> Result<DateTime<Local>> {
        self.cron()?
            .find_next_occurrence(&after, false)
            .map_err(|e| anyhow::anyhow!("cron `{}` has no next run: {e}", self.cron))
    }

    /// The latest firing at or before `at`.
    pub fn latest_at_or_before(&self, at: DateTime<Local>) -> Result<DateTime<Local>> {
        self.cron()?
            .find_previous_occurrence(&at, true)
            .map_err(|e| anyhow::anyhow!("cron `{}` has no previous run: {e}", self.cron))
    }

    pub fn problems(&self, config: &Config, path: Option<&Path>) -> Vec<String> {
        let mut out = Vec::new();
        if let Err(e) = validate_id(&self.id) {
            out.push(e);
        }
        if let Some(stem) = path.and_then(|p| p.file_stem()).and_then(|s| s.to_str())
            && stem != self.id
        {
            out.push(format!(
                "file name `{stem}.md` does not match id `{}`",
                self.id
            ));
        }
        if let Err(e) = self.cron() {
            out.push(e.to_string());
        }
        out.extend(prompt_problems(&self.prompt));
        out.extend(self.spec.problems(config));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    const SAMPLE: &str = "---
id: nightly-deps
cron: \"0 2 * * *\"
repo: /tmp
on_finish: pr
---

Update dependencies and run the tests.
";

    #[test]
    fn parses_and_computes_next_run() {
        let s = Schedule::parse(SAMPLE).unwrap();
        assert!(s.enabled);
        let from = Local.with_ymd_and_hms(2026, 9, 27, 10, 0, 0).unwrap();
        let next = s.next_after(from).unwrap();
        assert_eq!((next.hour(), next.minute()), (2, 0));
        assert!(next > from);
    }

    #[test]
    fn finds_latest_firing() {
        let s = Schedule::parse(SAMPLE).unwrap();
        let at = Local.with_ymd_and_hms(2026, 9, 27, 10, 0, 0).unwrap();
        let latest = s.latest_at_or_before(at).unwrap();
        assert_eq!(
            latest,
            Local.with_ymd_and_hms(2026, 9, 27, 2, 0, 0).unwrap()
        );
        assert_eq!(s.latest_at_or_before(latest).unwrap(), latest, "inclusive");
    }

    #[test]
    fn reports_bad_cron() {
        let s = Schedule::parse(&SAMPLE.replace("0 2 * * *", "every night")).unwrap();
        let problems = s.problems(&Config::default(), None);
        assert!(problems.iter().any(|p| p.contains("invalid cron")));
    }
}
