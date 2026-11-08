//! `furmi models`: what each configured agent's CLI says it can run.
//!
//! The same answer the dashboard's task form gets from `GET /api/models`, from
//! a terminal, so "why does the form offer nothing" has an answer here: the
//! agent that never names models, the CLI that failed, the command config.toml
//! doesn't set. Answers come from `models-cache.json` unless `--refresh`, and a
//! refresh writes them back, again exactly as the form does.

use anyhow::{Result, bail};

use crate::config::Config;
use crate::home::Home;
use crate::models::{Report, report};

#[derive(clap::Args)]
pub struct ModelsArgs {
    /// One agent from config.toml, instead of every configured one.
    #[arg(long)]
    agent: Option<String>,
    /// Ask the CLIs again instead of answering from (and leaving) the cache.
    #[arg(long)]
    refresh: bool,
}

pub fn models(home: &Home, args: ModelsArgs) -> Result<()> {
    let config = Config::load(&home.config_path())?;
    let names = match args.agent.as_deref().map(str::trim) {
        Some(named) if !named.is_empty() => vec![named.to_string()],
        _ => configured(&config),
    };
    for (i, name) in names.iter().enumerate() {
        let report = report(home, &config, Some(name), args.refresh);
        if !report.known {
            bail!(
                "unknown agent `{name}`: config.toml defines {}",
                configured(&config).join(", ")
            );
        }
        if i > 0 {
            println!();
        }
        println!("{name}  ({})", describe(&report));
        for model in &report.listed.models {
            println!("  {model}");
        }
    }
    Ok(())
}

/// The configured agents, built-in presets first: the same order `furmi agents`
/// lists them in.
fn configured(config: &Config) -> Vec<String> {
    crate::config::agent_statuses(config)
        .into_iter()
        .map(|a| a.name)
        .collect()
}

/// Why an agent has the list it has, in plain words.
fn describe(report: &Report) -> String {
    let listed = &report.listed;
    let mut parts = Vec::new();
    match (&listed.error, listed.source) {
        (Some(why), "cache") => {
            parts.push(format!("asking failed, so this is its last answer: {why}"))
        }
        (Some(why), _) => parts.push(format!("asking failed: {why}")),
        (None, "agent") => parts.push("asked just now".into()),
        (None, "cache") => parts.push("from the cache, kept for an hour".into()),
        (None, _) if listed.models.is_empty() => {
            parts.push("no `models_cmd` in config.toml: its CLI never names models".into())
        }
        (None, _) => {}
    }
    if !report.can_pin {
        parts.push("no `model_args`: a task can't pin a model on it".into());
    }
    if let Some(default) = &report.default {
        parts.push(format!("its default is {default}"));
    }
    parts.join("; ")
}
