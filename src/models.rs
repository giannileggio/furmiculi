//! Which models an agent can run, asked of the agent itself.
//!
//! The dashboard's task form offers a model for the same reason `furmi add`
//! takes `--model`: a task may want a bigger one than the agent's default. The
//! list of names belongs to the agent — it changes with the CLI's version and
//! with the providers you are signed in to — so furmi asks the CLI where the CLI
//! can answer ([`crate::config::Agent::models_cmd`], `pi --list-models`,
//! `opencode models`) and nowhere else. claude, codex, gemini and hermes take a
//! model name or show an interactive picker, and never name one.
//!
//! Asking costs seconds (pi takes ~4s over 50-odd models), so an answer is kept
//! in `models-cache.json` beside the tasks for an hour, and a CLI that fails
//! (opencode does, with a plugin of its own broken) falls back to the last
//! answer it gave instead of offering nothing.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Agent;
use crate::home::Home;

/// How long an answer stands before the CLI is asked again.
const TTL_SECS: i64 = 60 * 60;

/// How long a CLI gets to name its models. Generous: some of them reach the
/// provider's `/v1/models` before answering.
const TIMEOUT: Duration = Duration::from_secs(20);

/// An answer, and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    /// The model names, as that agent's own CLI spells them.
    pub models: Vec<String>,
    /// `agent` (asked just now), `cache` (asked before) or `none` (nothing was
    /// asked, or nothing to ask).
    pub source: &'static str,
    /// Why there is no answer, or why the one there is is old.
    pub error: Option<String>,
}

/// The models `agent` can run, asking `agent.models_cmd` unless a fresh answer
/// is already cached. Never an error: a form that can't be offered a list is
/// still a form you can type a model into.
pub fn list(home: &Home, name: &str, agent: &Agent, refresh: bool) -> Listed {
    if agent.models_cmd.is_empty() {
        return Listed {
            models: Vec::new(),
            source: "none",
            error: None,
        };
    }
    let mut cache = load(home);
    // A remembered answer for a *different* command says nothing about this one.
    let remembered = cache
        .agents
        .get(name)
        .filter(|e| e.cmd == agent.models_cmd)
        .cloned();
    if !refresh
        && let Some(entry) = remembered
            .as_ref()
            .filter(|e| (Utc::now() - e.at).num_seconds() < TTL_SECS)
    {
        return Listed {
            models: entry.models.clone(),
            source: "cache",
            error: None,
        };
    }
    match ask(&agent.models_cmd) {
        Ok(models) => {
            cache.agents.insert(
                name.to_string(),
                Entry {
                    cmd: agent.models_cmd.clone(),
                    at: Utc::now(),
                    models: models.clone(),
                },
            );
            save(home, &cache);
            Listed {
                models,
                source: "agent",
                error: None,
            }
        }
        // An old answer beats no answer: the models it named still exist, and
        // the form is a suggestion list, not a promise.
        Err(e) => {
            let models = remembered.map(|entry| entry.models).unwrap_or_default();
            Listed {
                // An empty `cache` would be a promise of nothing; `none` is honest.
                source: if models.is_empty() { "none" } else { "cache" },
                models,
                error: Some(format!("{e:#}")),
            }
        }
    }
}

fn ask(cmd: &[String]) -> Result<Vec<String>> {
    let (program, args) = cmd.split_first().context("`models_cmd` is empty")?;
    let mut command = Command::new(program);
    command.args(args);
    let out = crate::fsutil::run_with_timeout(command, TIMEOUT)
        .with_context(|| format!("asking `{}`", cmd.join(" ")))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.lines().next().unwrap_or_default().trim();
        bail!("`{}` failed: {why}", cmd.join(" "));
    }
    let models = parse(&String::from_utf8_lossy(&out.stdout));
    if models.is_empty() {
        bail!("`{}` named no models", cmd.join(" "));
    }
    Ok(models)
}

/// The model names in a CLI's answer, in the order it gave them.
///
/// Two shapes cover the agents furmi knows: one name per line, already
/// `provider/model` (`opencode models`), and a table whose first two columns are
/// the provider and the model (`pi --list-models`, and its header row). A
/// single column is taken as the model id on its own, which is what an agent
/// taking bare names (`--model sonnet`) would print.
fn parse(output: &str) -> Vec<String> {
    let mut models = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Bulleted lists, and the rules some CLIs draw under their header.
        let line = line.trim_start_matches(['-', '*', '•']).trim();
        if line.starts_with('=') {
            continue;
        }
        let columns: Vec<&str> = line.split_whitespace().collect();
        let name = match columns.as_slice() {
            [provider, model, ..] if is_header(provider) && is_header(model) => continue,
            [one] => (*one).to_string(),
            [provider, model, ..] => format!("{provider}/{model}"),
            [] => continue,
        };
        if !models.contains(&name) {
            models.push(name);
        }
    }
    models
}

/// A column title, not a model: the header row of a table.
fn is_header(column: &str) -> bool {
    matches!(
        column.to_ascii_lowercase().as_str(),
        "provider" | "name" | "id" | "model"
    )
}

// --- the cache ---------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    agents: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// The command that answered.
    cmd: Vec<String>,
    at: DateTime<Utc>,
    models: Vec<String>,
}

/// An unreadable or absent cache is an empty one: it is only ever a shortcut.
fn load(home: &Home) -> Cache {
    std::fs::read_to_string(home.models_cache())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save(home: &Home, cache: &Cache) {
    let Ok(text) = serde_json::to_string(cache) else {
        return;
    };
    let _ = std::fs::create_dir_all(home.root());
    let _ = crate::fsutil::write_atomic(&home.models_cache(), text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_table_with_a_header() {
        let out = "\
provider     model                  context  max-out  thinking  images
anthropic    claude-sonnet-5        1M       128K     yes       yes
ollama       hf.co/unsloth/Qwen3   131K     16K      yes       no
";
        assert_eq!(
            parse(out),
            ["anthropic/claude-sonnet-5", "ollama/hf.co/unsloth/Qwen3",]
        );
    }

    #[test]
    fn parses_one_name_per_line_and_leaves_provider_model_alone() {
        let out = "opencode-go/deepseek-v4.1\nopenai/gpt-5\n\n";
        assert_eq!(parse(out), ["opencode-go/deepseek-v4.1", "openai/gpt-5"]);
    }

    #[test]
    fn parses_a_bulleted_single_column() {
        let out = "* sonnet\n- opus\n= = = =\n";
        assert_eq!(parse(out), ["sonnet", "opus"]);
    }

    #[test]
    fn keeps_the_first_of_a_repeated_model() {
        let out = "a/one\na/one\nb/two\n";
        assert_eq!(parse(out), ["a/one", "b/two"]);
    }

    fn home_with_cache(cache: &str) -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().join("home"))).unwrap();
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.models_cache(), cache).unwrap();
        (dir, home)
    }

    fn agent(cmd: &[&str]) -> Agent {
        Agent {
            headless: vec!["x".into()],
            interactive: vec!["x".into()],
            model_args: vec!["--model".into(), "{model}".into()],
            model: None,
            models_cmd: cmd.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn an_agent_with_no_way_to_answer_offers_nothing() {
        let (_dir, home) = home_with_cache("{}");
        let listed = list(&home, "claude", &agent(&[]), false);
        assert_eq!(listed.source, "none");
        assert!(listed.models.is_empty());
        assert!(listed.error.is_none());
    }

    #[test]
    fn a_failing_command_falls_back_to_the_last_answer() {
        let (_dir, home) = home_with_cache(
            r#"{"agents":{"x":{"cmd":["false"],"at":"2000-01-01T00:00:00Z","models":["a/old"]}}}"#,
        );
        let listed = list(&home, "x", &agent(&["false"]), false);
        assert_eq!(listed.source, "cache");
        assert_eq!(listed.models, ["a/old"]);
        let error = listed.error.unwrap();
        assert!(error.contains("`false` failed"), "{error}");
    }

    #[test]
    fn a_failing_command_with_nothing_remembered_says_why() {
        let (_dir, home) = home_with_cache("{}");
        let listed = list(&home, "x", &agent(&["false"]), false);
        assert_eq!(listed.source, "none");
        assert!(listed.models.is_empty());
        assert!(listed.error.is_some());
    }

    #[test]
    fn asks_the_cli_and_remembers_the_answer() {
        let (_dir, home) = home_with_cache("{}");
        let cmd = ["printf", "p/one\np/two\n"];
        let listed = list(&home, "x", &agent(&cmd), false);
        assert_eq!(listed.source, "agent");
        assert_eq!(listed.models, ["p/one", "p/two"]);
        assert!(listed.error.is_none());
        // The second ask is answered from the cache, without running anything:
        // a command that would fail never gets the chance.
        let again = list(&home, "x", &agent(&cmd), false);
        assert_eq!(again.source, "cache");
        assert_eq!(again.models, ["p/one", "p/two"]);
        assert!(again.error.is_none(), "{again:?}");
    }

    #[test]
    fn a_changed_command_is_asked_again() {
        let (_dir, home) = home_with_cache("{}");
        list(&home, "x", &agent(&["printf", "old/model\n"]), false);
        let listed = list(&home, "x", &agent(&["printf", "new/model\n"]), false);
        assert_eq!(listed.source, "agent");
        assert_eq!(listed.models, ["new/model"]);
    }

    #[test]
    fn refresh_asks_again_before_the_hour_is_up() {
        let (_dir, home) = home_with_cache("{}");
        list(&home, "x", &agent(&["printf", "old/model\n"]), false);
        let listed = list(&home, "x", &agent(&["printf", "new/model\n"]), true);
        assert_eq!(listed.source, "agent");
        assert_eq!(listed.models, ["new/model"]);
    }

    #[test]
    fn a_corrupt_cache_is_simply_ignored() {
        let (_dir, home) = home_with_cache("not json");
        let listed = list(&home, "x", &agent(&["printf", "p/one\n"]), false);
        assert_eq!(listed.models, ["p/one"]);
        // And the answer it just got replaces the broken file.
        let saved = std::fs::read_to_string(home.models_cache()).unwrap();
        assert!(saved.contains("p/one"), "{saved}");
    }
}
