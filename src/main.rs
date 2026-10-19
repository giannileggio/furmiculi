mod clean;
mod cmd;
mod config;
mod control;
mod doctor;
mod edit;
mod exe;
mod frontmatter;
mod fsutil;
mod git;
mod home;
mod run;
mod schedule;
mod skill;
mod spec;
mod task;
mod tmux;
mod update_check;
mod watch;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::home::Home;

/// Queue and run coding-agent tasks defined as Markdown files.
#[derive(Parser)]
#[command(name = "furmi", version)]
struct Cli {
    /// Data folder [default: $FURMI_HOME or ~/.furmiculi]
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create the data folder, a config.toml, and the files agents use to
    /// write tasks. Safe to re-run: refreshes furmi's own files.
    Init(cmd::InitArgs),
    /// List tasks (or archived tasks, or schedules).
    Ls {
        #[arg(long, conflicts_with = "schedules")]
        archive: bool,
        #[arg(long)]
        schedules: bool,
        /// Refresh the listing instead of printing it once.
        #[arg(long)]
        watch: bool,
        /// Refresh interval when watching, e.g. 2s.
        #[arg(long, value_parser = humantime::parse_duration, default_value = "2s")]
        interval: std::time::Duration,
    },
    /// Create a new task file.
    Add(Box<cmd::AddArgs>),
    /// Check task/schedule files against the format. Checks all if none given.
    Validate { files: Vec<PathBuf> },
    /// Enqueue due schedules and run pending tasks in tmux, until interrupted.
    Run {
        /// Do a single pass and exit.
        #[arg(long)]
        once: bool,
    },
    /// Mark a task done: ends its session, runs on_finish, archives it.
    ///
    /// For running, needs_review or failed tasks. ID defaults to $FURMI_TASK_ID,
    /// which is set inside each task's window.
    Done { id: Option<String> },
    /// Mark a running or needs_review task failed, without retrying.
    Fail {
        id: Option<String>,
        /// Recorded as the task's `error`.
        #[arg(long, short)]
        reason: Option<String>,
    },
    /// Cancel a pending or running task. on_finish doesn't run.
    Cancel { id: Option<String> },
    /// Requeue a failed, cancelled or needs_review task, in the same worktree.
    Retry { id: String },
    /// Attach to a running task's tmux window (or the whole session).
    Attach { id: Option<String> },
    /// Print a task's captured output.
    ///
    /// The live tmux pane if it's still running, else the saved log for an
    /// attempt (the latest, by default).
    Logs {
        id: String,
        /// An earlier attempt, instead of the latest.
        #[arg(long)]
        attempt: Option<u32>,
    },
    /// Remove the worktrees of finished tasks (done only, unless --all).
    ///
    /// Worktrees with uncommitted changes are kept. Branches are never
    /// deleted. With --archive, also deletes the archived task file and its
    /// logs, once its worktree is gone.
    Clean(clean::CleanArgs),
    /// Serve a dashboard over the queue (with actions), until interrupted.
    Web(web::WebArgs),
    /// Run the queue and serve the dashboard together: the one-command way to
    /// get going. Opens the dashboard in your browser.
    Start(web::WebArgs),
    /// Show which coding agents are installed, how to get the others, and
    /// pick the default (`furmi agents use <name>`).
    Agents {
        #[command(subcommand)]
        action: Option<AgentsAction>,
    },
    /// Stop the runner that `furmi run` or `furmi start` began. Tasks already
    /// running keep going; start the runner again to resume the queue.
    Stop,
    /// Show whether the runner is alive, the queue, budget and failing schedules.
    Status,
    /// Check that tasks can run here: tools, config, agents, task files.
    Doctor,
    /// Print a systemd or launchd unit that keeps `furmi run` running.
    Service {
        #[arg(value_enum)]
        manager: doctor::Manager,
    },
    /// Launch a task's agent in the current process (used by `furmi run`).
    #[command(hide = true)]
    Exec {
        id: String,
        #[arg(long)]
        status_file: PathBuf,
    },
}

#[derive(Subcommand)]
enum AgentsAction {
    /// Make an agent the default for tasks that don't name one.
    Use { name: String },
}

fn main() -> ExitCode {
    // Before anything else can replace the binary on disk.
    exe::remember();
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let home = Home::resolve(cli.home)?;
    let Some(command) = cli.command else {
        update_check::check(&home);
        cmd::welcome(&home)?;
        return Ok(ExitCode::SUCCESS);
    };
    if !matches!(command, Command::Exec { .. }) {
        update_check::check(&home);
    }
    // Commands that make sense on a fresh machine set it up themselves.
    if matches!(
        command,
        Command::Ls { .. }
            | Command::Add(_)
            | Command::Run { .. }
            | Command::Web(_)
            | Command::Start(_)
            | Command::Status
            | Command::Agents { .. }
    ) {
        cmd::ensure_ready(&home)?;
    }
    match command {
        Command::Init(args) => cmd::init(&home, args)?,
        Command::Ls {
            archive,
            schedules,
            watch,
            interval,
        } => cmd::ls(&home, archive, schedules, watch.then_some(interval))?,
        Command::Add(args) => cmd::add(&home, *args)?,
        Command::Validate { files } => return cmd::validate(&home, files),
        Command::Run { once } => run::run(&home, once)?,
        Command::Done { id } => control::done(&home, id)?,
        Command::Fail { id, reason } => control::fail(&home, id, reason)?,
        Command::Cancel { id } => control::cancel(&home, id)?,
        Command::Retry { id } => control::retry(&home, &id)?,
        Command::Attach { id } => control::attach(&home, id)?,
        Command::Logs { id, attempt } => print!("{}", control::logs(&home, &id, attempt)?),
        Command::Clean(args) => clean::clean(&home, args)?,
        Command::Web(args) => web::serve(&home, args)?,
        Command::Start(args) => web::start(&home, args)?,
        Command::Status => doctor::status(&home)?,
        Command::Agents { action } => match action {
            None => cmd::agents(&home)?,
            Some(AgentsAction::Use { name }) => cmd::use_agent(&home, &name)?,
        },
        Command::Stop => match control::stop_runner(&home)? {
            Some(pid) => {
                println!("Stopped the runner (pid {pid}). Tasks already running keep going.")
            }
            None => println!("The runner isn't running."),
        },
        Command::Doctor => return doctor::doctor(&home),
        Command::Service { manager } => doctor::service(&home, manager)?,
        Command::Exec { id, status_file } => return run::exec(&home, &id, &status_file),
    }
    Ok(ExitCode::SUCCESS)
}
