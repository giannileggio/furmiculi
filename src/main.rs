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
mod models;
mod models_cmd;
mod nodes;
mod notify;
mod restart;
mod run;
mod schedule;
mod schedule_cmd;
mod skill;
mod spec;
mod task;
mod template;
mod template_cmd;
mod tmux;
mod update;
mod update_check;
mod version;
mod watch;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
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
    /// Manage schedules: list, add, edit, pause, resume, delete.
    #[command(subcommand)]
    Schedule(schedule_cmd::ScheduleCmd),
    /// Templates: tasks to start new tasks from. List, save, delete.
    #[command(subcommand)]
    Template(template_cmd::TemplateCmd),
    /// Queue a draft: it is picked up by the runner from now on.
    ///
    /// A draft is a task made with `furmi add --draft` (or Duplicate, or
    /// Draft in the dashboard) that waits for you to look it over first.
    Queue {
        id: String,
        /// Start time, RFC 3339 (2026-09-28T02:00:00+02:00).
        #[arg(long, conflicts_with = "delay")]
        at: Option<chrono::DateTime<chrono::FixedOffset>>,
        /// Start after a delay, e.g. 2h.
        #[arg(long = "in", value_parser = humantime::parse_duration)]
        delay: Option<std::time::Duration>,
    },
    /// Copy any task (waiting or finished) into a new draft.
    Duplicate {
        id: String,
        /// Id of the copy [default: derived from the prompt].
        #[arg(long = "as")]
        new_id: Option<String>,
    },
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
    /// Other machines whose queues this dashboard shows alongside its own.
    #[command(subcommand)]
    Node(nodes::NodeCmd),
    /// Remove the worktrees of finished tasks (done only, unless --all).
    ///
    /// Worktrees with uncommitted changes are kept, and so are worktrees git
    /// can't inspect (--remove-broken takes those too). Branches are never
    /// deleted. With --archive, also deletes the archived task file and its
    /// logs, once its worktree is gone.
    Clean(clean::CleanArgs),
    /// Serve a dashboard over the queue (with actions), until interrupted
    /// (or in the background, with --detach).
    ///
    /// The first run asks once where to serve from — your tailnet, or just
    /// this machine — and saves the answer in config.toml; `--setup` asks
    /// again.
    Web(web::WebArgs),
    /// Run the queue and serve the dashboard together: the one-command way to
    /// get going. Opens the dashboard in your browser. With --detach it runs
    /// in the background and hands the terminal back.
    Start(web::WebArgs),
    /// Show which coding agents are installed, how to get the others, and
    /// pick the default (`furmi agents use <name>`).
    Agents {
        #[command(subcommand)]
        action: Option<AgentsAction>,
    },
    /// Print the models each configured agent's CLI says it can run (the list
    /// the dashboard's task form offers), and keep the answers in
    /// models-cache.json.
    ///
    /// --refresh asks the CLIs again and rewrites the cache; without it an
    /// answer from the last hour is reused. One agent at a time with --agent.
    Models(models_cmd::ModelsArgs),
    /// Stop the dashboard and the runner, whichever of `furmi start`,
    /// `furmi web` and `furmi run` began them. Tasks already running keep
    /// going; start again to resume the queue.
    Stop,
    /// Pause the runner without stopping it: nothing new starts and no
    /// schedule fires. Running tasks carry on and are still finished. Lasts
    /// across runner restarts, until `furmi resume`.
    Pause,
    /// Undo `furmi pause`: the runner starts queued tasks and schedules again.
    Resume,
    /// Stop the dashboard and the runner and start them again the way they
    /// were started, so they run the installed binary (after `furmi update`).
    ///
    /// Tasks already running keep going in tmux and the new runner picks them
    /// back up. Under `furmi service` the service manager restarts them
    /// instead.
    Restart,
    /// Show whether the runner is alive, the queue and failing schedules.
    Status,
    /// Install a new release over this one, keeping the same install.
    ///
    /// Only for installs made by the release installer, which is what leaves
    /// the receipt this reads. `--check` says whether a newer release exists
    /// without installing anything.
    Update {
        /// Say whether a newer release exists, and install nothing.
        #[arg(long)]
        check: bool,
        /// Install this version instead of the latest, e.g. 0.4.2 (going back
        /// to an earlier one works too).
        #[arg(long, value_name = "VERSION")]
        to: Option<String>,
        /// Then run `furmi restart`, so the dashboard and runner use the new
        /// release. Without it they keep running the old one and `update` says so.
        #[arg(long, conflicts_with = "check")]
        restart: bool,
    },
    /// Check that tasks can run here: tools, config, agents, task files.
    Doctor,
    /// Print systemd or launchd units that keep `furmi run` and the dashboard
    /// (`furmi web`) running.
    Service {
        #[arg(value_enum)]
        manager: doctor::Manager,
    },
    /// Launch a task's agent or command in the current process (used by `furmi run`).
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
    // No point nagging about a new release in the command that installs one.
    if !matches!(command, Command::Exec { .. } | Command::Update { .. }) {
        update_check::check(&home);
    }
    // Commands that make sense on a fresh machine set it up themselves.
    if matches!(
        command,
        Command::Ls { .. }
            | Command::Add(_)
            | Command::Template(_)
            | Command::Run { .. }
            | Command::Web(_)
            | Command::Start(_)
            | Command::Status
            | Command::Agents { .. }
            | Command::Models(_)
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
        Command::Schedule(command) => schedule_cmd::run(&home, command)?,
        Command::Template(command) => template_cmd::run(&home, command)?,
        Command::Queue { id, at, delay } => {
            let start = match (at, delay) {
                (Some(at), _) => edit::Start::At(at),
                (None, Some(delay)) => edit::Start::At(
                    task::now()
                        + chrono::Duration::from_std(delay).context("that delay is too long")?,
                ),
                (None, None) => edit::Start::Keep,
            };
            control::queue(&home, &id, start)?;
            println!("Queued `{id}`");
        }
        Command::Duplicate { id, new_id } => {
            let copy = control::duplicate(&home, &id, new_id)?;
            println!("Saved a draft, `{copy}`. `furmi queue {copy}` starts it.");
        }
        Command::Validate { files } => return cmd::validate(&home, files),
        Command::Run { once } => run::run(&home, once)?,
        Command::Done { id } => control::done(&home, id)?,
        Command::Fail { id, reason } => control::fail(&home, id, reason)?,
        Command::Cancel { id } => control::cancel(&home, id)?,
        Command::Retry { id } => control::retry(&home, &id)?,
        Command::Attach { id } => control::attach(&home, id)?,
        Command::Logs { id, attempt } => print!("{}", control::logs(&home, &id, attempt)?),
        Command::Node(cmd) => nodes::run(&home, cmd)?,
        Command::Clean(args) => clean::clean(&home, args)?,
        Command::Web(args) => web::serve(&home, args)?,
        Command::Start(args) => web::start(&home, args)?,
        Command::Status => doctor::status(&home)?,
        Command::Agents { action } => match action {
            None => cmd::agents(&home)?,
            Some(AgentsAction::Use { name }) => cmd::use_agent(&home, &name)?,
        },
        Command::Models(args) => models_cmd::models(&home, args)?,
        Command::Stop => {
            // A `furmi start` dashboard stops the runner it began as it goes,
            // so ask for the runner only if there was one to begin with.
            let had_runner = home.runner_alive();
            match control::stop_dashboard(&home)? {
                Some(pid) => println!("Stopped the dashboard (pid {pid})."),
                None => println!("No dashboard is running."),
            }
            if had_runner {
                match control::stop_runner(&home)? {
                    Some(pid) => println!(
                        "Stopped the runner (pid {pid}). Tasks already running keep going."
                    ),
                    None => println!("The runner stopped with the dashboard."),
                }
            } else {
                println!("The runner isn't running.");
            }
        }
        Command::Pause => {
            if home.set_paused(true)? {
                println!(
                    "Paused: nothing new starts. Running tasks carry on. `furmi resume` undoes it."
                );
            } else {
                println!("Already paused.");
            }
        }
        Command::Resume => {
            if home.set_paused(false)? {
                println!("Resumed.");
            } else {
                println!("Not paused.");
            }
        }
        Command::Restart => restart::restart(&home)?,
        Command::Update { check, to, restart } => {
            update::update(&home, update::UpdateArgs { check, to, restart })?
        }
        Command::Doctor => return doctor::doctor(&home),
        Command::Service { manager } => doctor::service(&home, manager)?,
        Command::Exec { id, status_file } => return run::exec(&home, &id, &status_file),
    }
    Ok(ExitCode::SUCCESS)
}
