use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local, SubsecRound};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::frontmatter;
use crate::fsutil;
use crate::spec::{SPEC_KEYS, TaskSpec, validate_id};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
    /// An interactive session ended without signalling done/fail.
    NeedsReview,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::NeedsReview => "needs_review",
        }
    }
}

/// One task file: `tasks/<id>.md`. The Markdown body is the prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    #[serde(default)]
    pub status: Status,
    #[serde(flatten)]
    pub spec: TaskSpec,
    /// Don't start before this time. Absent means as soon as possible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_at: Option<DateTime<FixedOffset>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<FixedOffset>>,
    /// Free text, e.g. `cli`, `agent`, `schedule:<id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,

    // Written by furmiculi only.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u32,
    /// Attempts cut short by something other than the task itself (tmux or
    /// the machine going away). They don't count against `retries`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub interruptions: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<FixedOffset>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<FixedOffset>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Total input tokens the agent reported, if it reports usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    /// Total output tokens the agent reported, if it reports usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    /// Cost in USD the agent reported, if it reports usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// The model the agent said it ran with, if it says (`provider/model` for
    /// the agents that report both). A task that doesn't pin `model` still ran
    /// with one — this is that answer, kept so a finished task can be read
    /// without digging through its log. Written by furmiculi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,

    #[serde(skip)]
    pub prompt: String,
}

const TASK_KEYS: &[&str] = &[
    "id",
    "status",
    "scheduled_at",
    "created_at",
    "created_by",
    "attempts",
    "interruptions",
    "started_at",
    "finished_at",
    "exit_code",
    "tmux_window",
    "error",
    "tokens_in",
    "tokens_out",
    "cost_usd",
    "model_used",
];

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Current local time, to the second (keeps the files readable).
pub fn now() -> DateTime<FixedOffset> {
    Local::now().fixed_offset().trunc_subsecs(0)
}

/// `"1.2k in / 340 out"`, or `"-"` if the agent didn't report token usage.
pub fn fmt_tokens(tokens_in: Option<u64>, tokens_out: Option<u64>) -> String {
    match (tokens_in, tokens_out) {
        (None, None) => "-".into(),
        (i, o) => format!(
            "{} in / {} out",
            i.map_or("-".into(), fmt_count),
            o.map_or("-".into(), fmt_count)
        ),
    }
}

fn fmt_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// `"$0.0512"`, or `"-"` if the agent didn't report a cost.
pub fn fmt_cost(cost_usd: Option<f64>) -> String {
    cost_usd.map_or_else(|| "-".into(), |c| format!("${c:.4}"))
}

impl Task {
    pub fn new(id: String, spec: TaskSpec, prompt: String) -> Self {
        Self {
            id,
            status: Status::Pending,
            spec,
            scheduled_at: None,
            created_at: Some(now()),
            created_by: None,
            attempts: 0,
            interruptions: 0,
            started_at: None,
            finished_at: None,
            exit_code: None,
            tmux_window: None,
            error: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            model_used: None,
            prompt,
        }
    }

    /// Adds one attempt's reported usage to the task's totals.
    pub fn add_usage(
        &mut self,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        cost: Option<f64>,
    ) {
        fn sum<T: std::ops::Add<Output = T> + Default + Copy>(
            a: Option<T>,
            b: Option<T>,
        ) -> Option<T> {
            match (a, b) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or_default() + b.unwrap_or_default()),
            }
        }
        self.tokens_in = sum(self.tokens_in, tokens_in);
        self.tokens_out = sum(self.tokens_out, tokens_out);
        self.cost_usd = sum(self.cost_usd, cost);
    }

    /// The model to name for this task, and where the name came from.
    ///
    /// A finished task is asked first what it actually ran with: a task that
    /// pins no model still runs with whatever its agent defaults to, and only
    /// the agent's own output knows which. Before it has run there is nothing
    /// to report, so the task's own `model`, then its agent's configured one,
    /// is the best answer. `"-"` and `"none"` when nothing says.
    pub fn model_in_use(&self, config: &Config) -> (String, &'static str) {
        let eff = self.spec.resolve(&config.defaults);
        let agent_default = eff
            .agent
            .as_ref()
            .and_then(|name| config.agents.get(name))
            .and_then(|agent| agent.model.clone());
        let finished = !matches!(self.status, Status::Pending | Status::Running);
        for (model, source) in [
            (
                if finished {
                    self.model_used.clone()
                } else {
                    None
                },
                "used",
            ),
            (eff.model.clone(), "task"),
            (agent_default, "agent"),
        ] {
            if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
                return (model, source);
            }
        }
        ("-".into(), "none")
    }

    /// Records the model an attempt said it ran with. The first answer wins:
    /// a retry with a broken model that reports nothing shouldn't erase what
    /// the working attempt ran with, and the pinned `model` is never touched.
    pub fn set_model_used(&mut self, model: Option<&str>) {
        if self.model_used.is_none()
            && let Some(model) = model.map(str::trim).filter(|m| !m.is_empty())
        {
            self.model_used = Some(model.to_string());
        }
    }

    pub fn parse(content: &str) -> Result<Self> {
        let allowed: Vec<&str> = TASK_KEYS.iter().chain(SPEC_KEYS).copied().collect();
        let (mut task, body): (Task, String) = frontmatter::parse(content, &allowed)?;
        task.prompt = body;
        Ok(task)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn to_markdown(&self) -> Result<String> {
        frontmatter::render(self, &self.prompt)
    }

    /// Writes the task, failing if the file already exists.
    pub fn create(&self, path: &Path) -> Result<()> {
        fsutil::create_atomic(path, self.to_markdown()?)
    }

    /// Replaces the file in one step, so a crash can't leave it truncated.
    pub fn save(&self, path: &Path) -> Result<()> {
        fsutil::write_atomic(path, self.to_markdown()?)
    }

    /// Branch the task runs on: explicit, or `furmi/<id>` when using a worktree.
    pub fn effective_branch(&self, config: &Config) -> Option<String> {
        let eff = self.spec.resolve(&config.defaults);
        eff.branch
            .or_else(|| eff.worktree.then(|| format!("furmi/{}", self.id)))
    }

    /// Problems that make this task unrunnable. `path`, if given, is checked
    /// against the id.
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
        out.extend(prompt_problems(&self.prompt));
        out.extend(self.spec.problems(config));
        out
    }
}

/// The prompt is passed to the agent as one command-line argument, which
/// Linux caps at 128 KiB (and fails to exec beyond that).
pub const MAX_PROMPT_BYTES: usize = 100_000;

/// Problems with a task or schedule prompt.
pub fn prompt_problems(prompt: &str) -> Option<String> {
    if prompt.is_empty() {
        Some("empty prompt: write the instructions below the frontmatter".into())
    } else if prompt.len() > MAX_PROMPT_BYTES {
        Some(format!(
            "prompt is {} bytes, over the {MAX_PROMPT_BYTES} an agent can be given as an argument: \
             put the details in a file in the repo and point the prompt at it",
            prompt.len()
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---
id: fix-login
repo: /tmp
branch: fix/login
on_finish: pr
retries: 3
timeout: 30m
scheduled_at: 2026-09-28T02:00:00+02:00
---

Fix the redirect loop.
";

    #[test]
    fn parses_sample() {
        let t = Task::parse(SAMPLE).unwrap();
        assert_eq!(t.id, "fix-login");
        assert_eq!(t.status, Status::Pending);
        assert_eq!(t.spec.retries, Some(3));
        assert_eq!(t.spec.timeout, Some(std::time::Duration::from_secs(1800)));
        assert_eq!(t.prompt, "Fix the redirect loop.");
        assert!(t.scheduled_at.is_some());
    }

    #[test]
    fn roundtrips() {
        let t = Task::parse(SAMPLE).unwrap();
        let again = Task::parse(&t.to_markdown().unwrap()).unwrap();
        assert_eq!(t, again);
    }

    #[test]
    fn rejects_unknown_field() {
        let bad = SAMPLE.replace("retries: 3", "retires: 3");
        assert!(Task::parse(&bad).is_err());
    }

    #[test]
    fn requires_repo() {
        let bad = SAMPLE.replace("repo: /tmp\n", "");
        assert!(Task::parse(&bad).is_err());
    }

    #[test]
    fn default_branch_for_worktrees() {
        let mut t = Task::parse(SAMPLE).unwrap();
        let c = Config::default();
        assert_eq!(t.effective_branch(&c).as_deref(), Some("fix/login"));
        t.spec.branch = None;
        assert_eq!(t.effective_branch(&c).as_deref(), Some("furmi/fix-login"));
        t.spec.worktree = Some(false);
        assert_eq!(t.effective_branch(&c), None);
    }

    #[test]
    fn reports_mismatched_file_name() {
        let t = Task::parse(SAMPLE).unwrap();
        let problems = t.problems(&Config::default(), Some(Path::new("tasks/other.md")));
        assert!(problems.iter().any(|p| p.contains("does not match id")));
    }

    #[test]
    fn formats_tokens() {
        assert_eq!(fmt_tokens(None, None), "-");
        assert_eq!(fmt_tokens(Some(120), Some(80)), "120 in / 80 out");
        assert_eq!(
            fmt_tokens(Some(1500), Some(2_340_000)),
            "1.5k in / 2.3M out"
        );
    }

    #[test]
    fn usage_adds_up_across_attempts() {
        let mut t = Task::parse(SAMPLE).unwrap();
        t.add_usage(None, None, None);
        assert_eq!((t.tokens_in, t.cost_usd), (None, None));
        t.add_usage(Some(100), Some(10), Some(0.5));
        t.add_usage(Some(50), None, Some(0.25));
        assert_eq!(t.tokens_in, Some(150));
        assert_eq!(t.tokens_out, Some(10));
        assert_eq!(t.cost_usd, Some(0.75));
    }

    #[test]
    fn the_model_in_use_is_what_ran_then_what_is_pinned_then_the_agents_default() {
        let mut t = Task::parse(SAMPLE).unwrap();
        let mut config = Config::default();
        assert_eq!(t.model_in_use(&config), ("-".into(), "none"));

        // The agent's configured model, for a task that pins none.
        config.defaults.agent = Some("pi".into());
        config.agents.insert(
            "pi".into(),
            crate::config::Agent {
                headless: Vec::new(),
                interactive: Vec::new(),
                model_args: Vec::new(),
                model: Some("agent/default".into()),
            },
        );
        assert_eq!(t.model_in_use(&config), ("agent/default".into(), "agent"));

        // A pinned model beats it.
        t.spec.model = Some("pinned/model".into());
        assert_eq!(t.model_in_use(&config), ("pinned/model".into(), "task"));

        // Waiting: what it will run with, not what an earlier attempt reported.
        t.model_used = Some("reported/model".into());
        assert_eq!(t.model_in_use(&config), ("pinned/model".into(), "task"));

        // Finished: the agent's own answer is the truth.
        t.status = Status::Done;
        t.finished_at = Some(now());
        assert_eq!(t.model_in_use(&config), ("reported/model".into(), "used"));
        t.model_used = None;
        assert_eq!(t.model_in_use(&config), ("pinned/model".into(), "task"));
    }

    #[test]
    fn the_first_model_a_task_reports_is_kept_and_never_overwrites_a_pin() {
        let mut t = Task::parse(SAMPLE).unwrap();
        t.set_model_used(None);
        assert_eq!(t.model_used, None);
        t.set_model_used(Some("  "));
        assert_eq!(t.model_used, None);
        t.set_model_used(Some(" opencode-go/deepseek-v4.1-flash "));
        assert_eq!(
            t.model_used.as_deref(),
            Some("opencode-go/deepseek-v4.1-flash")
        );
        // A later attempt that reports another model doesn't rewrite history.
        t.set_model_used(Some("other/model"));
        assert_eq!(
            t.model_used.as_deref(),
            Some("opencode-go/deepseek-v4.1-flash")
        );
        // And it is a field of its own: the task still pins what it pinned.
        assert_eq!(t.spec.model, None);

        // It survives the round trip through the file.
        t.spec.model = Some("pinned".into());
        let again = Task::parse(&t.to_markdown().unwrap()).unwrap();
        assert_eq!(again.model_used, t.model_used);
        assert_eq!(again.spec.model.as_deref(), Some("pinned"));
    }

    #[test]
    fn oversized_prompts_are_a_problem() {
        let mut t = Task::parse(SAMPLE).unwrap();
        t.prompt = "x".repeat(MAX_PROMPT_BYTES + 1);
        let problems = t.problems(&Config::default(), None);
        assert!(problems.iter().any(|p| p.contains("bytes")), "{problems:?}");
    }

    #[test]
    fn formats_cost() {
        assert_eq!(fmt_cost(None), "-");
        assert_eq!(fmt_cost(Some(0.0512)), "$0.0512");
    }

    /// A failed save (full disk, read-only folder) must leave the
    /// task file exactly as it was: the atomic write fails before
    /// the rename, so the old file survives intact. A directory
    /// planted where the temp file would go fails the write the
    /// same way a full disk does.
    #[test]
    fn a_failed_save_does_not_corrupt_the_task_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.md");
        let task = Task::parse(SAMPLE).unwrap();
        task.save(&path).unwrap();

        let planted = fsutil::temp_path(&path);
        std::fs::create_dir(&planted).unwrap();
        let mut changed = task.clone();
        changed.status = Status::Running;
        assert!(changed.save(&path).is_err());

        let saved = Task::load(&path).unwrap();
        assert_eq!(saved, task);
        std::fs::remove_dir(&planted).unwrap();
    }
}
