use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::spec::{Mode, OnFinish};

/// Written by `furmi init`, with `{agent}` replaced by [`default_config_toml`].
/// Must stay in sync with `Config::default()` (enforced by a test).
const CONFIG_TEMPLATE: &str = r#"# furmiculi configuration.
# Every value below is the built-in default; delete what you don't change.

[runner]
max_parallel = 3            # tasks running at once, overall
max_parallel_per_repo = 1   # tasks running at once in the same repo
tmux_session = "furmi"      # one session, one window per task
poll_interval = "30s"       # how often `furmi run` rescans the folders

# Fallbacks for any field a task or schedule doesn't set.
[defaults]
{agent}
mode = "headless"           # headless | interactive
worktree = true             # run each task in its own git worktree
on_finish = "none"          # none | commit | push | pr
retries = 1                 # extra attempts after a failure
retry_delay = "5m"
timeout = "2h"

# How to launch each agent, as argv lists (no shell involved).
# Placeholders: {prompt} {id}. `model_args` is appended only when a task (or
# the agent's `model = "..."`) sets a model, filling in {model}; without one
# the agent uses its own default. Add [agents.<name>] tables for any other
# CLI agent; these presets stay available unless you redefine them.
#
# Headless runs can't answer permission prompts: add each agent's
# auto-approve flags (e.g. claude "--permission-mode", "acceptEdits";
# codex "--full-auto"; gemini "--yolo") to suit your trust level.
#
# claude's headless preset asks for `--output-format json`, and pi's for
# `--mode json`: furmi parses those for token usage (see
# `tokens_in`/`tokens_out` in FORMAT.md), and pi's event stream is
# the only place it says a run failed. Drop the flag and furmi just won't record
# usage for that agent (pi's log also goes back to plain text).
[agents.claude]
headless = ["claude", "-p", "{prompt}", "--output-format", "json"]
interactive = ["claude", "{prompt}"]
model_args = ["--model", "{model}"]

[agents.codex]
headless = ["codex", "exec", "{prompt}"]
interactive = ["codex", "{prompt}"]
model_args = ["--model", "{model}"]

[agents.gemini]
headless = ["gemini", "--prompt", "{prompt}"]
interactive = ["gemini", "--prompt-interactive", "{prompt}"]
model_args = ["--model", "{model}"]

[agents.opencode]
headless = ["opencode", "run", "{prompt}"]
interactive = ["opencode", "--prompt", "{prompt}"]
model_args = ["--model", "{model}"]

[agents.pi]
headless = ["pi", "--mode", "json", "{prompt}"]
interactive = ["pi", "{prompt}"]
model_args = ["--model", "{model}"]

# The dashboard (`furmi web`, `furmi start`). The first `furmi web` asks once
# whether to serve this machine only or your other machines too (a tailnet),
# and writes this table, so it asks only that once. Flags beat it.
# [web]
# configured = true                       # the questions have been answered
# listen = "0.0.0.0:7433"               # ...to serve other machines
# allowed_hosts = ["this-box.ts.net"]   # the Host header those browsers send
# token_file = "~/.furmiculi/web-token" # shared secret every request carries
"#;

/// The config `furmi init` writes, with `agent` as the default agent (left
/// commented out if none was chosen).
pub fn default_config_toml(agent: Option<&str>) -> String {
    let line = match agent {
        Some(a) => format!("{:<28}# any [agents.*] below", format!("agent = \"{a}\"")),
        None => format!(
            "{:<28}# any [agents.*] below; tasks without one fail",
            "# agent = \"<name>\""
        ),
    };
    CONFIG_TEMPLATE.replace("{agent}", &line)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub runner: Runner,
    pub defaults: Defaults,
    pub agents: BTreeMap<String, Agent>,
    pub web: Web,
}

/// The dashboard, as `furmi web` / `furmi start` serve it. All optional: with
/// nothing set the dashboard listens on loopback, where the per-run token and
/// loopback checks are enough. A wide `listen` additionally needs
/// `allowed_hosts` and a `token_file` (or the same as flags) — `furmi web
/// --setup` fills all three in once.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Web {
    /// Whether the questions `furmi web --setup` asks have been answered —
    /// "this machine only" included. While this is false (and nothing else
    /// here is set), the first interactive `furmi web` asks, then saves it.
    pub configured: bool,
    /// Address to bind, e.g. `0.0.0.0:7433`. Unset: `127.0.0.1:<--port>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// Exact `Host` headers a non-loopback bind accepts. `host` or `host:port`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    /// File holding the shared bearer token for a non-loopback bind. `~` is
    /// expanded; the file must be 0600.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Runner {
    pub max_parallel: usize,
    pub max_parallel_per_repo: usize,
    pub tmux_session: String,
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    pub agent: Option<String>,
    pub mode: Mode,
    pub worktree: bool,
    pub on_finish: OnFinish,
    pub retries: u32,
    #[serde(with = "humantime_serde")]
    pub retry_delay: Duration,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub headless: Vec<String>,
    pub interactive: Vec<String>,
    /// Appended when a model is set, with `{model}` filled in.
    #[serde(default)]
    pub model_args: Vec<String>,
    /// Model used when the task doesn't name one.
    #[serde(default)]
    pub model: Option<String>,
}

impl Default for Runner {
    fn default() -> Self {
        Self {
            max_parallel: 3,
            max_parallel_per_repo: 1,
            tmux_session: "furmi".into(),
            poll_interval: Duration::from_secs(30),
        }
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            agent: None,
            mode: Mode::Headless,
            worktree: true,
            on_finish: OnFinish::None,
            retries: 1,
            retry_delay: Duration::from_secs(5 * 60),
            timeout: Duration::from_secs(2 * 60 * 60),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            runner: Runner::default(),
            defaults: Defaults::default(),
            agents: builtin_agents(),
            web: Web::default(),
        }
    }
}

/// Presets for common CLI agents: `(name, headless, interactive)`. All take
/// `--model`. The headless ones ask for JSON output where the agent has it
/// (`claude --output-format json`, `pi --mode json`), because that is what
/// carries the token usage furmi records — and, for pi, the only place a
/// failed run is reported (its JSON mode exits 0 either way).
const PRESETS: &[(&str, &[&str], &[&str])] = &[
    (
        "claude",
        &["claude", "-p", "{prompt}", "--output-format", "json"],
        &["claude", "{prompt}"],
    ),
    (
        "codex",
        &["codex", "exec", "{prompt}"],
        &["codex", "{prompt}"],
    ),
    (
        "gemini",
        &["gemini", "--prompt", "{prompt}"],
        &["gemini", "--prompt-interactive", "{prompt}"],
    ),
    (
        "opencode",
        &["opencode", "run", "{prompt}"],
        &["opencode", "--prompt", "{prompt}"],
    ),
    (
        "pi",
        &["pi", "--mode", "json", "{prompt}"],
        &["pi", "{prompt}"],
    ),
];

fn builtin_agents() -> BTreeMap<String, Agent> {
    let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect();
    PRESETS
        .iter()
        .map(|(name, headless, interactive)| {
            let agent = Agent {
                headless: argv(headless),
                interactive: argv(interactive),
                model_args: argv(&["--model", "{model}"]),
                model: None,
            };
            (name.to_string(), agent)
        })
        .collect()
}

/// How to install each preset's CLI. All are npm packages, checked to exist
/// and to provide the program the preset runs.
const INSTALL_HINTS: &[(&str, &str)] = &[
    ("claude", "npm install -g @anthropic-ai/claude-code"),
    ("codex", "npm install -g @openai/codex"),
    ("gemini", "npm install -g @google/gemini-cli"),
    ("opencode", "npm install -g opencode-ai"),
    ("pi", "npm install -g @mariozechner/pi-coding-agent"),
];

/// One agent as the setup views show it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AgentStatus {
    pub name: String,
    /// The program that runs it, e.g. `claude`.
    pub program: String,
    pub installed: bool,
    pub is_default: bool,
    /// A command that installs it, for the built-in presets.
    pub install: Option<&'static str>,
}

/// Every configured agent, built-in presets first in their usual order.
pub fn agent_statuses(config: &Config) -> Vec<AgentStatus> {
    let order = |name: &str| {
        PRESETS
            .iter()
            .position(|(p, _, _)| *p == name)
            .unwrap_or(usize::MAX)
    };
    let mut agents: Vec<AgentStatus> = config
        .agents
        .iter()
        .map(|(name, agent)| {
            let program = agent.headless.first().cloned().unwrap_or_default();
            AgentStatus {
                installed: on_path(&program) || Path::new(&program).is_file(),
                is_default: config.defaults.agent.as_deref() == Some(name.as_str()),
                install: INSTALL_HINTS
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, cmd)| *cmd),
                name: name.clone(),
                program,
            }
        })
        .collect();
    agents.sort_by_key(|a| (order(&a.name), a.name.clone()));
    agents
}

/// Makes `name` the default agent in the config file, leaving the rest of it
/// (comments, ordering, the user's own edits) as it was. Refuses names the
/// config doesn't define, and never writes a file that wouldn't parse.
pub fn set_default_agent(path: &Path, name: &str) -> Result<()> {
    let text = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        default_config_toml(None)
    };
    let config = Config::parse(&text)?;
    if !config.agents.contains_key(name) {
        let known: Vec<_> = config.agents.keys().map(String::as_str).collect();
        bail!("unknown agent `{name}` (configured: {})", known.join(", "));
    }
    let updated = with_default_agent(&text, name);
    let check = Config::parse(&updated)?;
    if check.defaults.agent.as_deref() != Some(name) {
        bail!("couldn't set the default agent in {}", path.display());
    }
    crate::fsutil::write_atomic(path, updated)
}

/// `text` with `agent = "<name>"` in its `[defaults]` table: replacing the
/// existing line (or, failing that, a commented-out one), else adding one.
fn with_default_agent(text: &str, name: &str) -> String {
    let line = format!(
        "{:<28}# any [agents.*] below\n",
        format!("agent = \"{name}\"")
    );
    let sets_agent =
        |s: &str| s.starts_with("agent") && s["agent".len()..].trim_start().starts_with('=');
    // An `agent` key, or a commented-out one like the template's example.
    let is_active = |l: &str| sets_agent(l.trim_start());
    let is_example = |l: &str| {
        let t = l.trim_start();
        t.starts_with('#')
            && !t.starts_with("##")
            && sets_agent(t.trim_start_matches('#').trim_start())
    };

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    // Which lines are inside [defaults].
    let mut in_defaults = Vec::with_capacity(lines.len());
    let mut inside = false;
    for l in &lines {
        let t = l.trim_start();
        if t.starts_with('[') {
            inside = t.trim_end().starts_with("[defaults]");
        }
        in_defaults.push(inside);
    }
    let has_active = lines
        .iter()
        .zip(&in_defaults)
        .any(|(l, d)| *d && is_active(l));
    let target = lines.iter().zip(&in_defaults).position(|(l, d)| {
        *d && if has_active {
            is_active(l)
        } else {
            is_example(l)
        }
    });

    let mut out = String::new();
    match target {
        Some(at) => {
            for (i, l) in lines.iter().enumerate() {
                out.push_str(if i == at { &line } else { l });
            }
        }
        None => match lines.iter().position(|l| l.trim_end() == "[defaults]") {
            // Right under the header, where the other defaults are.
            Some(header) => {
                for (i, l) in lines.iter().enumerate() {
                    out.push_str(l);
                    if i == header {
                        if !l.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push_str(&line);
                    }
                }
            }
            None => {
                out.push_str(text);
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("\n[defaults]\n");
                out.push_str(&line);
            }
        },
    }
    out
}

/// Saves the dashboard's answers in the config file, leaving the rest of it
/// (comments, ordering, the user's own edits) as it was. What `furmi web
/// --setup` calls once; after that a plain `furmi web` reads this back.
/// Refuses to write a file that wouldn't parse or wouldn't round-trip.
pub fn set_web_config(path: &Path, web: &Web) -> Result<()> {
    let text = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        default_config_toml(None)
    };
    // Parse first: saving a dashboard setting isn't worth clobbering a config
    // the user is still editing.
    Config::parse(&text)?;
    let updated = with_web(&text, web);
    let check = Config::parse(&updated)?;
    if &check.web != web {
        bail!("couldn't save the [web] settings in {}", path.display());
    }
    crate::fsutil::write_atomic(path, updated)
}

/// `text` with a `[web]` table carrying `web`: an existing table is replaced
/// in place (whatever sits above it kept), else one is appended.
fn with_web(text: &str, web: &Web) -> String {
    let block = web_table(web);
    let mut out = String::with_capacity(text.len() + block.len());
    let mut wrote = false;
    let mut dropping = false;
    for line in text.lines() {
        let t = line.trim();
        if dropping {
            if t.starts_with('[') {
                dropping = false;
            } else {
                continue;
            }
        }
        if !wrote && t == "[web]" {
            out.push_str(&block);
            wrote = true;
            dropping = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !wrote {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&block);
    }
    out
}

/// The `[web]` table as TOML text, keys rendered by the serializer so nothing
/// needs quoting by hand. The comment sits inside the table (not above it), so
/// replacing the table on a later `--setup` replaces it too instead of piling
/// up copies.
fn web_table(web: &Web) -> String {
    let body = toml::to_string(web).unwrap_or_default();
    format!("[web]\n# Written by `furmi web --setup`; flags beat these.\n{body}")
}

/// Preset agents whose program is on `$PATH`, in preset order.
pub fn installed_presets() -> Vec<&'static str> {
    PRESETS
        .iter()
        .filter(|(_, headless, _)| on_path(headless[0]))
        .map(|(name, _, _)| *name)
        .collect()
}

pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

impl Config {
    /// Loads `path`, or the defaults if it doesn't exist.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut config: Config = toml::from_str(text)?;
        // Built-in agents stay available unless overridden by name.
        for (name, agent) in builtin_agents() {
            config.agents.entry(name).or_insert(agent);
        }
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> Result<()> {
        if self.runner.max_parallel == 0 || self.runner.max_parallel_per_repo == 0 {
            bail!("runner.max_parallel and runner.max_parallel_per_repo must be at least 1");
        }
        for (name, agent) in &self.agents {
            if agent.headless.is_empty() || agent.interactive.is_empty() {
                bail!("agents.{name}: `headless` and `interactive` must not be empty");
            }
        }
        if let Some(agent) = &self.defaults.agent
            && !self.agents.contains_key(agent)
        {
            bail!("defaults.agent `{agent}` is not defined under [agents]");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sets_the_default_agent_in_place() {
        // The template, with and without an agent chosen.
        for original in [
            default_config_toml(None),
            default_config_toml(Some("claude")),
        ] {
            let updated = with_default_agent(&original, "codex");
            assert_eq!(
                updated
                    .lines()
                    .filter(|l| l.starts_with("agent = "))
                    .count(),
                1
            );
            assert!(updated.contains("agent = \"codex\""));
            assert!(!updated.contains("# agent = \"<name>\""));
            // Everything else is as it was.
            let strip = |t: &str| {
                t.lines()
                    .filter(|l| !l.contains("agent = \"") && !l.contains("# agent ="))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            assert_eq!(strip(&original), strip(&updated));
            assert_eq!(
                Config::parse(&updated).unwrap().defaults.agent.as_deref(),
                Some("codex")
            );
        }
    }

    #[test]
    fn prefers_the_active_line_over_a_commented_example() {
        let text =
            "[defaults]\n# agent = \"<name>\"\nagent = \"claude\"  # mine\nmode = \"headless\"\n";
        let updated = with_default_agent(text, "pi");
        assert_eq!(updated.matches("agent = ").count(), 2, "{updated}");
        assert!(updated.contains("# agent = \"<name>\"\n"), "{updated}");
        assert!(Config::parse(&updated).is_ok(), "{updated}");
        assert_eq!(
            Config::parse(&updated).unwrap().defaults.agent.as_deref(),
            Some("pi")
        );
    }

    #[test]
    fn adds_the_agent_when_there_is_no_line_or_no_table() {
        let no_line = "[runner]\nmax_parallel = 2\n\n[defaults]\nmode = \"headless\"\n\n[agents.x]\nheadless = [\"x\"]\ninteractive = [\"x\"]\n";
        let updated = with_default_agent(no_line, "claude");
        let config = Config::parse(&updated).unwrap();
        assert_eq!(config.defaults.agent.as_deref(), Some("claude"));
        assert_eq!(config.runner.max_parallel, 2);

        for text in [
            "",
            "[runner]\nmax_parallel = 2",
            "[runner]\nmax_parallel = 2\n",
        ] {
            let updated = with_default_agent(text, "claude");
            let config = Config::parse(&updated).unwrap();
            assert_eq!(
                config.defaults.agent.as_deref(),
                Some("claude"),
                "{updated}"
            );
        }
        // A key named `agent` in another table isn't touched.
        let other = "[agents.x]\nheadless = [\"x\"]\ninteractive = [\"x\"]\nmodel = \"m\"\n";
        let updated = with_default_agent(other, "x");
        assert!(updated.contains("model = \"m\""));
        assert_eq!(
            Config::parse(&updated).unwrap().defaults.agent.as_deref(),
            Some("x")
        );
    }

    #[test]
    fn set_default_agent_validates_and_writes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // No config yet: it's created from the template.
        set_default_agent(&path, "gemini").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().defaults.agent.as_deref(),
            Some("gemini")
        );

        let before = std::fs::read_to_string(&path).unwrap();
        let err = set_default_agent(&path, "nonesuch")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown agent"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // A custom agent defined in the file can be chosen too.
        std::fs::write(
            &path,
            format!("{before}\n[agents.mine]\nheadless = [\"mine\", \"{{prompt}}\"]\ninteractive = [\"mine\"]\n"),
        )
        .unwrap();
        set_default_agent(&path, "mine").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().defaults.agent.as_deref(),
            Some("mine")
        );

        // A file that doesn't parse is reported, not overwritten.
        std::fs::write(&path, "this is [not toml").unwrap();
        assert!(set_default_agent(&path, "claude").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is [not toml");
    }

    #[test]
    fn agent_statuses_list_presets_in_order_with_install_commands() {
        let config = Config::parse("[defaults]\nagent = \"codex\"\n").unwrap();
        let agents = agent_statuses(&config);
        let names: Vec<_> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["claude", "codex", "gemini", "opencode", "pi"]);
        assert!(agents.iter().all(|a| a.install.is_some()));
        assert_eq!(agents.iter().filter(|a| a.is_default).count(), 1);
        assert!(
            agents
                .iter()
                .find(|a| a.name == "codex")
                .unwrap()
                .is_default
        );
    }

    use super::*;

    #[test]
    fn web_settings_are_optional_and_strict() {
        let c = Config::parse(
            "[web]\nlisten = \"0.0.0.0:7433\"\nallowed_hosts = [\"box.ts.net\"]\n\
             token_file = \"~/.furmiculi/web-token\"\n",
        )
        .unwrap();
        assert_eq!(c.web.listen.as_deref(), Some("0.0.0.0:7433"));
        assert_eq!(c.web.allowed_hosts, ["box.ts.net"]);
        assert_eq!(
            c.web.token_file.as_deref(),
            Some(Path::new("~/.furmiculi/web-token"))
        );
        // Nothing set: loopback, as before [web] existed.
        assert_eq!(Config::default().web, Web::default());
        // "This machine only", answered: just the marker.
        let only = Config::parse("[web]\nconfigured = true\n").unwrap();
        assert!(only.web.configured);
        assert_eq!(only.web.listen, None);
        // A typo inside [web] is caught like anywhere else.
        assert!(Config::parse("[web]\nallowed_host = [\"box.ts.net\"]\n").is_err());
    }

    #[test]
    fn saves_the_web_settings_without_touching_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let web = Web {
            configured: true,
            listen: Some("0.0.0.0:7433".into()),
            allowed_hosts: vec!["box.ts.net".into(), "100.64.0.10".into()],
            token_file: Some(PathBuf::from("~/.furmiculi/web-token")),
        };

        // No config yet: created from the template, comments and all.
        set_web_config(&path, &web).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("# furmiculi configuration."), "{first}");
        assert_eq!(Config::load(&path).unwrap().web, web);
        assert_eq!(
            first.lines().filter(|l| l.trim() == "[web]").count(),
            1,
            "{first}"
        );
        // The template's commented-out example is left for the reader.
        assert!(first.contains("# [web]\n"), "{first}");

        // Changing it replaces the table rather than adding a second one, and
        // writing the same settings twice doesn't grow the file.
        let changed = Web {
            allowed_hosts: vec!["other.ts.net".into()],
            ..web.clone()
        };
        set_web_config(&path, &changed).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            second.lines().filter(|l| l.trim() == "[web]").count(),
            1,
            "{second}"
        );
        assert!(second.contains("# furmiculi configuration."), "{second}");
        assert_eq!(Config::load(&path).unwrap().web, changed);
        set_web_config(&path, &changed).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), second);

        // A file that doesn't parse is reported, not overwritten.
        std::fs::write(&path, "this is [not toml").unwrap();
        assert!(set_web_config(&path, &web).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is [not toml");
    }

    #[test]
    fn default_template_matches_builtin_defaults() {
        assert_eq!(
            Config::parse(&default_config_toml(None)).unwrap(),
            Config::default()
        );
        let with_agent = Config::parse(&default_config_toml(Some("codex"))).unwrap();
        assert_eq!(with_agent.defaults.agent.as_deref(), Some("codex"));
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let c = Config::parse("[runner]\nmax_parallel = 8\n").unwrap();
        assert_eq!(c.runner.max_parallel, 8);
        assert_eq!(c.runner.tmux_session, "furmi");
        assert_eq!(c.agents.len(), PRESETS.len());
    }

    #[test]
    fn custom_agents_are_added_to_builtins() {
        let c = Config::parse(
            "[agents.aider]\nheadless = [\"aider\", \"--message\", \"{prompt}\"]\ninteractive = [\"aider\"]\n",
        )
        .unwrap();
        assert!(c.agents.contains_key("claude"));
        assert!(c.agents["aider"].model_args.is_empty());
    }

    #[test]
    fn rejects_typos() {
        assert!(Config::parse("[runner]\nmax_paralel = 2\n").is_err());
    }
}
