//! `furmi schedule`: list, add, edit, pause, resume and delete schedules from
//! the terminal. Everything that writes a schedule file goes through
//! `edit.rs`, the same code the dashboard uses, so the files match and are
//! validated exactly as `furmi validate` would.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use chrono::Local;
use clap::Subcommand;

use crate::cmd;
use crate::edit;
use crate::home::{Home, contract_tilde};
use crate::schedule::Schedule;
use crate::spec::{Mode, OnFinish, TaskSpec};

#[derive(Subcommand)]
pub enum ScheduleCmd {
    /// List schedules with their next run (same as `furmi ls --schedules`).
    Ls,
    /// Create a schedule: a task template that runs on a cron expression.
    Add(Box<AddArgs>),
    /// Change a schedule. Only the options you give change; the rest stays.
    Edit(Box<EditArgs>),
    /// Stop a schedule from firing, keeping its file.
    Pause { id: String },
    /// Let a paused schedule fire again, from its next slot (missed ones are
    /// not made up).
    Resume { id: String },
    /// Delete a schedule. Tasks it already queued stay.
    Delete { id: String },
}

#[derive(clap::Args)]
pub struct AddArgs {
    /// What the agent should do each time. Asked for if omitted (or read from
    /// stdin when piped).
    #[arg(conflicts_with = "prompt")]
    text: Option<String>,
    /// 5-field cron expression in local time, e.g. "0 2 * * *".
    #[arg(long)]
    cron: String,
    /// Repository the tasks run in [default: the git repo you're in].
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Schedule id (also the file name). Derived from the prompt if omitted.
    #[arg(long)]
    id: Option<String>,
    /// The prompt, as an option instead of the positional argument.
    #[arg(long, short)]
    prompt: Option<String>,
    /// Enqueue even if the previous run is still pending or running.
    #[arg(long)]
    allow_overlap: bool,
    #[command(flatten)]
    spec: SpecArgs,
}

#[derive(clap::Args)]
pub struct EditArgs {
    id: String,
    /// New prompt, as an option instead of the positional argument.
    #[arg(conflicts_with = "prompt")]
    text: Option<String>,
    #[arg(long, short)]
    prompt: Option<String>,
    /// New cron expression, e.g. "0 2 * * *".
    #[arg(long)]
    cron: Option<String>,
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Enqueue even if the previous run is still pending or running.
    #[arg(long, overrides_with = "no_allow_overlap")]
    allow_overlap: bool,
    /// Skip a firing while the previous run is still pending or running.
    #[arg(long)]
    no_allow_overlap: bool,
    #[command(flatten)]
    spec: SpecArgs,
}

/// The task options a schedule shares with `furmi add`.
#[derive(clap::Args)]
struct SpecArgs {
    /// Run this shell command instead of an agent: a command task, with no
    /// prompt, agent, model or mode. Runs in the repo itself unless --worktree.
    #[arg(long, conflicts_with_all = ["text", "prompt", "agent", "model", "mode"])]
    run: Option<String>,
    #[arg(long)]
    branch: Option<String>,
    /// Run in a dedicated worktree, for a task that wouldn't by default.
    #[arg(long, conflicts_with = "no_worktree")]
    worktree: bool,
    /// Run directly in the repo instead of a dedicated worktree.
    #[arg(long)]
    no_worktree: bool,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    #[arg(long, value_enum)]
    on_finish: Option<OnFinish>,
    #[arg(long)]
    retries: Option<u32>,
    /// e.g. 5m, 1h
    #[arg(long, value_parser = humantime::parse_duration)]
    retry_delay: Option<Duration>,
    /// e.g. 30m, 2h
    #[arg(long, value_parser = humantime::parse_duration)]
    timeout: Option<Duration>,
}

impl SpecArgs {
    fn is_empty(&self) -> bool {
        self.run.is_none()
            && self.branch.is_none()
            && !self.worktree
            && !self.no_worktree
            && self.agent.is_none()
            && self.model.is_none()
            && self.mode.is_none()
            && self.on_finish.is_none()
            && self.retries.is_none()
            && self.retry_delay.is_none()
            && self.timeout.is_none()
    }

    /// Overwrites the fields that were given.
    fn apply(self, spec: &mut TaskSpec) {
        if self.run.is_some() {
            spec.run = self.run;
        }
        if self.branch.is_some() {
            spec.branch = self.branch;
        }
        if let Some(worktree) = cmd::worktree_choice(self.worktree, self.no_worktree) {
            spec.worktree = Some(worktree);
        }
        if self.agent.is_some() {
            spec.agent = self.agent;
        }
        if self.model.is_some() {
            spec.model = self.model;
        }
        if self.mode.is_some() {
            spec.mode = self.mode;
        }
        if self.on_finish.is_some() {
            spec.on_finish = self.on_finish;
        }
        if self.retries.is_some() {
            spec.retries = self.retries;
        }
        if self.retry_delay.is_some() {
            spec.retry_delay = self.retry_delay;
        }
        if self.timeout.is_some() {
            spec.timeout = self.timeout;
        }
    }
}

pub fn run(home: &Home, command: ScheduleCmd) -> Result<()> {
    cmd::ensure_ready(home)?;
    match command {
        ScheduleCmd::Ls => cmd::ls(home, false, true, None),
        ScheduleCmd::Add(args) => add(home, *args),
        ScheduleCmd::Edit(args) => change(home, *args),
        ScheduleCmd::Pause { id } => {
            edit::set_schedule_enabled(home, &id, false)?;
            println!("Paused `{id}`");
            Ok(())
        }
        ScheduleCmd::Resume { id } => {
            edit::set_schedule_enabled(home, &id, true)?;
            println!("Resumed `{id}`");
            Ok(())
        }
        ScheduleCmd::Delete { id } => {
            edit::delete_schedule(home, &id)?;
            println!("Deleted `{id}`");
            Ok(())
        }
    }
}

fn add(home: &Home, args: AddArgs) -> Result<()> {
    let prompt = if args.spec.run.is_some() {
        String::new()
    } else {
        cmd::read_prompt(args.text.or(args.prompt))?
    };
    let repo = match args.repo {
        Some(repo) => repo,
        None => cmd::current_repo()?,
    };
    let mut spec = edit::blank_spec(repo);
    args.spec.apply(&mut spec);
    let id = edit::add_schedule(home, args.id, args.cron, args.allow_overlap, spec, prompt)?;
    let path = home.schedules().join(format!("{id}.md"));
    if std::io::stdout().is_terminal() {
        println!("Added schedule `{id}`");
        let s = Schedule::load(&path)?;
        if let Ok(next) = s.next_after(Local::now()) {
            println!("  next run   {}", next.format("%Y-%m-%d %H:%M"));
        }
        println!("  repo       {}", contract_tilde(&s.spec.repo));
    } else {
        // Scripts and agents read the path of the new file.
        println!("{}", path.display());
    }
    Ok(())
}

fn change(home: &Home, args: EditArgs) -> Result<()> {
    let prompt = args.text.or(args.prompt);
    let overlap = match (args.allow_overlap, args.no_allow_overlap) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    if prompt.is_none()
        && args.cron.is_none()
        && args.repo.is_none()
        && overlap.is_none()
        && args.spec.is_empty()
    {
        bail!("nothing to change: give a new prompt or an option such as --cron");
    }
    edit::update_schedule(home, &args.id, |s| {
        if let Some(prompt) = prompt {
            s.prompt = prompt;
        }
        if let Some(cron) = args.cron {
            s.cron = cron;
        }
        if let Some(repo) = args.repo {
            // Same normalisation as `add`: `~/x` or a relative path would
            // otherwise be stored as typed.
            s.spec.repo = edit::path_of(&repo.to_string_lossy())?;
        }
        if let Some(overlap) = overlap {
            s.allow_overlap = overlap;
        }
        args.spec.apply(&mut s.spec);
        Ok(())
    })?;
    println!("Updated `{}`", args.id);
    Ok(())
}
