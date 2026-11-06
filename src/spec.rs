//! Fields shared by tasks and schedules: what to run and how.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, Defaults};
use crate::git;
use crate::home::expand_tilde;
use crate::task::MAX_PROMPT_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Non-interactive run; the exit code decides done/failed.
    Headless,
    /// Interactive session; completion is signalled with `furmi done|fail`.
    Interactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum OnFinish {
    None,
    Commit,
    Push,
    Pr,
}

/// Every field except `repo` is optional and falls back to `[defaults]`
/// in config.toml.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    /// Empty only in a template that leaves the project to whoever uses it.
    #[serde(
        serialize_with = "serialize_repo",
        skip_serializing_if = "path_is_empty"
    )]
    pub repo: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_finish: Option<OnFinish>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    #[serde(
        default,
        with = "humantime_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub retry_delay: Option<Duration>,
    #[serde(
        default,
        with = "humantime_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
    /// A shell command to run instead of an agent, which makes this a command
    /// task: it has no prompt, agent, model or mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

fn path_is_empty(path: &std::path::Path) -> bool {
    path.as_os_str().is_empty()
}

fn serialize_repo<S: serde::Serializer>(path: &PathBuf, s: S) -> Result<S::Ok, S::Error> {
    path.serialize(s)
}

pub const SPEC_KEYS: &[&str] = &[
    "repo",
    "branch",
    "worktree",
    "agent",
    "model",
    "mode",
    "on_finish",
    "retries",
    "retry_delay",
    "timeout",
    "run",
];

/// A spec with all defaults applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub repo: PathBuf,
    pub branch: Option<String>,
    pub worktree: bool,
    pub agent: Option<String>,
    /// The task's model. The agent's own `model` applies if this is unset.
    pub model: Option<String>,
    pub mode: Mode,
    pub on_finish: OnFinish,
    pub retries: u32,
    pub retry_delay: Duration,
    pub timeout: Duration,
    /// The shell command of a command task.
    pub run: Option<String>,
}

impl Effective {
    /// What carries the task out, for listings: `command`, else its agent.
    pub fn runner(&self) -> &str {
        match (&self.run, &self.agent) {
            (Some(_), _) => "command",
            (None, Some(agent)) => agent,
            (None, None) => "-",
        }
    }
}

impl TaskSpec {
    /// What the task asks for, given its body: its command, else its prompt.
    pub fn ask<'a>(&'a self, prompt: &'a str) -> &'a str {
        self.run.as_deref().unwrap_or(prompt)
    }

    /// The `[defaults]` that describe an agent's work don't reach a command
    /// task: it has no agent, and a default `on_finish` would commit whatever
    /// is in the checkout it runs in by default. Set here, they apply.
    pub fn resolve(&self, defaults: &Defaults) -> Effective {
        let command = self.run.is_some();
        Effective {
            repo: expand_tilde(&self.repo),
            branch: self.branch.clone(),
            worktree: self.worktree.unwrap_or(!command && defaults.worktree),
            agent: self
                .agent
                .clone()
                .or_else(|| defaults.agent.clone())
                .filter(|_| !command),
            model: self.model.clone(),
            mode: if command {
                Mode::Headless
            } else {
                self.mode.unwrap_or(defaults.mode)
            },
            on_finish: self.on_finish.unwrap_or(if command {
                OnFinish::None
            } else {
                defaults.on_finish
            }),
            retries: self.retries.unwrap_or(defaults.retries),
            retry_delay: self.retry_delay.unwrap_or(defaults.retry_delay),
            timeout: self.timeout.unwrap_or(defaults.timeout),
            run: self.run.clone(),
        }
    }

    /// Problems that make this spec unrunnable under `config`.
    pub fn problems(&self, config: &Config) -> Vec<String> {
        let mut out = Vec::new();
        let eff = self.resolve(&config.defaults);

        if eff.repo.is_relative() {
            out.push(format!(
                "repo `{}` must be an absolute path or start with `~`",
                self.repo.display()
            ));
        } else if !eff.repo.is_dir() {
            out.push(format!("repo `{}` does not exist", eff.repo.display()));
        } else if !eff.repo.join(".git").exists() && (eff.worktree || eff.branch.is_some()) {
            out.push(format!(
                "repo `{}` is not a git repository, but a branch/worktree was requested",
                eff.repo.display()
            ));
        }
        if let Some(run) = &self.run {
            out.extend(self.command_problems(run));
            self.push_common_problems(&eff, &mut out);
            return out;
        }
        let known = || {
            let names: Vec<_> = config.agents.keys().map(String::as_str).collect();
            names.join(", ")
        };
        match eff.agent.as_deref().map(|a| (a, config.agents.get(a))) {
            None => out.push(format!(
                "no agent: set `agent` (one of {}) here or under [defaults] in config.toml",
                known()
            )),
            Some((name, None)) => out.push(format!(
                "unknown agent `{name}`: add an [agents.{name}] section to config.toml, \
                 or use one of: {}",
                known()
            )),
            Some((name, Some(agent))) => {
                if eff.model.is_some() && agent.model_args.is_empty() {
                    out.push(format!(
                        "agent `{name}` has no `model_args` in config.toml, so `model` can't be passed to it"
                    ));
                }
            }
        }
        self.push_common_problems(&eff, &mut out);
        out
    }

    fn push_common_problems(&self, eff: &Effective, out: &mut Vec<String>) {
        if let Some(b) = &self.branch
            && !git::valid_branch_name(b)
        {
            out.push(format!("invalid branch name `{b}`"));
        }
        if eff.on_finish == OnFinish::Pr && eff.branch.is_none() && !eff.worktree {
            out.push("on_finish: pr needs a branch or a worktree".into());
        }
    }

    fn command_problems(&self, run: &str) -> Vec<String> {
        let mut out = Vec::new();
        if run.trim().is_empty() {
            out.push("`run` is empty: give it the command to run".into());
        } else if run.len() > MAX_PROMPT_BYTES {
            out.push(format!(
                "`run` is {} bytes, over the {MAX_PROMPT_BYTES} a shell can be given as an \
                 argument: put the commands in a script and run that",
                run.len()
            ));
        }
        for (field, set) in [
            ("agent", self.agent.is_some()),
            ("model", self.model.is_some()),
            ("mode", self.mode.is_some()),
        ] {
            if set {
                out.push(format!(
                    "`{field}` can't be set with `run`: a command task runs no agent"
                ));
            }
        }
        out
    }
}

/// Ids are used as file names and tmux window names: keep them boring.
pub fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid id `{id}`: use 1-64 chars of a-z, 0-9 and `-`, starting with a letter or digit"
        ))
    }
}

/// Turns free text into an id candidate, e.g. "Fix the login!" -> "fix-the-login".
pub fn slugify(text: &str, max_len: usize) -> String {
    let mut slug = String::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_ascii_lowercase();
        let extra = if slug.is_empty() {
            word.len()
        } else {
            word.len() + 1
        };
        if slug.len() + extra > max_len {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    if slug.is_empty() { "task".into() } else { slug }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_truncates_on_word_boundary() {
        assert_eq!(
            slugify("Fix the login redirect!", 64),
            "fix-the-login-redirect"
        );
        assert_eq!(slugify("Fix the login redirect", 12), "fix-the");
        assert_eq!(slugify("¿¿??", 64), "task");
    }

    #[test]
    fn id_validation() {
        assert!(validate_id("nightly-deps-2").is_ok());
        assert!(validate_id("Bad Id").is_err());
        assert!(validate_id("-leading").is_err());
        assert!(validate_id("").is_err());
    }

    #[test]
    fn resolve_applies_defaults() {
        let spec = TaskSpec {
            repo: "/tmp".into(),
            branch: None,
            worktree: None,
            agent: None,
            model: Some("custom".into()),
            mode: None,
            on_finish: None,
            retries: Some(5),
            retry_delay: None,
            timeout: None,
            run: None,
        };
        let d = Defaults {
            agent: Some("codex".into()),
            ..Defaults::default()
        };
        let eff = spec.resolve(&d);
        assert_eq!(eff.model.as_deref(), Some("custom"));
        assert_eq!(eff.retries, 5);
        assert_eq!(eff.agent.as_deref(), Some("codex"));
        assert_eq!(eff.timeout, d.timeout);
    }

    #[test]
    fn agent_problems() {
        let spec = TaskSpec {
            repo: "/tmp".into(),
            branch: None,
            worktree: Some(false),
            agent: None,
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
            run: None,
        };
        let mut config = Config::default();
        assert!(spec.problems(&config)[0].starts_with("no agent"));

        config.agents.get_mut("codex").unwrap().model_args.clear();
        let spec = TaskSpec {
            agent: Some("codex".into()),
            model: Some("m".into()),
            ..spec
        };
        assert!(spec.problems(&config)[0].contains("no `model_args`"));
        let spec = TaskSpec {
            agent: Some("nope".into()),
            ..spec
        };
        assert!(spec.problems(&config)[0].starts_with("unknown agent"));
    }

    fn command_spec(run: &str) -> TaskSpec {
        TaskSpec {
            run: Some(run.into()),
            ..crate::edit::blank_spec("/tmp".into())
        }
    }

    #[test]
    fn a_command_task_takes_no_agent_defaults_and_stays_in_the_checkout() {
        let defaults = Defaults {
            agent: Some("codex".into()),
            mode: Mode::Interactive,
            on_finish: OnFinish::Pr,
            retries: 4,
            ..Defaults::default()
        };
        let eff = command_spec("make").resolve(&defaults);
        assert_eq!(eff.agent, None);
        assert_eq!(eff.runner(), "command");
        assert_eq!(eff.mode, Mode::Headless);
        assert!(!eff.worktree);
        assert_eq!(eff.on_finish, OnFinish::None);
        // What isn't about an agent still falls back.
        assert_eq!(eff.retries, 4);

        // Asked for by name, they apply.
        let spec = TaskSpec {
            worktree: Some(true),
            on_finish: Some(OnFinish::Commit),
            ..command_spec("make")
        };
        let eff = spec.resolve(&defaults);
        assert!(eff.worktree);
        assert_eq!(eff.on_finish, OnFinish::Commit);
    }

    #[test]
    fn a_command_task_needs_no_agent_and_rejects_one() {
        let mut config = Config::default();
        config.defaults.agent = None;
        assert_eq!(command_spec("make").problems(&config), Vec::<String>::new());

        for (spec, field) in [
            (
                TaskSpec {
                    agent: Some("codex".into()),
                    ..command_spec("make")
                },
                "`agent`",
            ),
            (
                TaskSpec {
                    model: Some("m".into()),
                    ..command_spec("make")
                },
                "`model`",
            ),
            (
                TaskSpec {
                    mode: Some(Mode::Headless),
                    ..command_spec("make")
                },
                "`mode`",
            ),
        ] {
            let problems = spec.problems(&config);
            assert!(problems.iter().any(|p| p.contains(field)), "{problems:?}");
        }
        assert!(command_spec("  ").problems(&config)[0].contains("empty"));
        let long = "x".repeat(MAX_PROMPT_BYTES + 1);
        assert!(command_spec(&long).problems(&config)[0].contains("bytes"));
    }

    #[test]
    fn rejects_branch_names_git_would_refuse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let spec = TaskSpec {
            repo: dir.path().into(),
            branch: Some("bad branch".into()),
            worktree: Some(false),
            agent: Some("codex".into()),
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
            run: None,
        };
        let config = Config::default();
        assert!(
            spec.problems(&config)
                .iter()
                .any(|p| p.contains("invalid branch name"))
        );

        let spec = TaskSpec {
            branch: Some("fix/login".into()),
            ..spec
        };
        assert!(spec.problems(&config).is_empty());
    }
}
