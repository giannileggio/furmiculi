use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, Local};

use crate::config::{Config, default_config_toml, installed_presets};
use crate::frontmatter;
use crate::home::{Home, contract_tilde, expand_tilde};
use crate::schedule::Schedule;
use crate::skill;
use crate::spec::{Mode, OnFinish, TaskSpec, slugify, validate_id};
use crate::task::{Status, Task, fmt_tokens, now};
use crate::template::{self, Template};

#[derive(clap::Args)]
pub struct InitArgs {
    /// Default agent for tasks that don't name one: any [agents.*] preset.
    /// Asked (or detected from $PATH) if omitted. Only used for a new
    /// config.toml.
    #[arg(long)]
    agent: Option<String>,
    /// Make the furmi-tasks skill available in every project.
    #[arg(long, overrides_with = "no_global_skill")]
    global_skill: bool,
    /// Don't, and remove the links a previous --global-skill made.
    #[arg(long)]
    no_global_skill: bool,
}

pub fn init(home: &Home, args: InitArgs) -> Result<()> {
    let global = match (args.global_skill, args.no_global_skill) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    setup(home, args.agent, Some(global))?;
    println!("initialized {}", home.root().display());
    Ok(())
}

/// Creates the data folder, config and agent files. `global_skill` is what
/// to do about the global skill links: `None` leaves them alone, `Some(c)`
/// applies `skill::set_global` with choice `c`.
fn setup(home: &Home, agent: Option<String>, global_skill: Option<Option<bool>>) -> Result<()> {
    for dir in home.dirs() {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let config = home.config_path();
    if config.exists() {
        skill::note(format_args!("kept existing {}", config.display()));
    } else {
        let agent = choose_agent(agent)?;
        crate::fsutil::write_atomic(&config, default_config_toml(agent.as_deref()))?;
        skill::note(format_args!("wrote {}", config.display()));
    }
    template::install_starters(home)?;
    skill::install_in_home(home)?;
    if let Some(choice) = global_skill {
        skill::set_global(home, choice)?;
    }
    Ok(())
}

/// Sets the data folder up on first use, so nobody has to know about
/// `furmi init`. Asks nothing but which agent to default to when several are
/// installed (and only on a terminal); the global-skill question is left to
/// an explicit `furmi init`.
pub fn ensure_ready(home: &Home) -> Result<()> {
    if home.ensure_initialized().is_ok() {
        // A home made before templates existed gets the starters once.
        return template::install_starters(home);
    }
    // Guidance, not output: stdout stays clean for scripts reading `furmi add`.
    eprintln!(
        "First run: setting up {} (this only happens once)",
        contract_tilde(home.root())
    );
    skill::set_quiet(true);
    let result = setup(home, None, None);
    skill::set_quiet(false);
    result?;
    eprintln!("Tip: `furmi init` can also let your coding agents queue tasks from any repo.\n");
    Ok(())
}

/// `furmi agents`: which agents are installed, and how to get the others.
pub fn agents(home: &Home) -> Result<()> {
    let config = Config::load(&home.config_path())?;
    let statuses = crate::config::agent_statuses(&config);
    let mut rows = vec![row(["AGENT", "STATUS", "HOW TO INSTALL"])];
    for a in &statuses {
        let status = match (a.installed, a.is_default) {
            (true, true) => "installed (default)",
            (true, false) => "installed",
            (false, true) => "not installed (default)",
            (false, false) => "not installed",
        };
        rows.push(vec![
            a.name.clone(),
            status.into(),
            if a.installed {
                String::new()
            } else {
                a.install.unwrap_or("see its documentation").into()
            },
        ]);
    }
    print_table(&rows);
    println!();
    if statuses.iter().any(|a| a.installed) {
        println!("Pick the default for new tasks with: furmi agents use <name>");
    } else {
        println!(
            "Install one with the command shown (it needs Node.js), run it once in a terminal \
             to sign in, then pick it with: furmi agents use <name>"
        );
    }
    Ok(())
}

/// `furmi agents use <name>`: sets `agent` under `[defaults]` in config.toml.
pub fn use_agent(home: &Home, name: &str) -> Result<()> {
    let config = Config::load(&home.config_path())?;
    crate::config::set_default_agent(&home.config_path(), name)?;
    println!("`{name}` is now the default agent.");
    if let Some(a) = crate::config::agent_statuses(&config)
        .iter()
        .find(|a| a.name == name)
        && !a.installed
    {
        println!(
            "Note: `{}` isn't installed yet; tasks will fail until it is.{}",
            a.program,
            a.install
                .map(|cmd| format!(" Install it with: {cmd}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// What a bare `furmi` shows: where things stand and the next thing to try.
pub fn welcome(home: &Home) -> Result<()> {
    println!(
        "furmiculi (ants, in Sicilian): queue coding-agent tasks and let the colony run them while you're away.\n"
    );
    if home.ensure_initialized().is_ok() {
        crate::doctor::status(home)?;
        println!();
    }
    println!("Try:");
    println!("  furmi add \"Fix the login redirect\"   queue a task for the repo you're in");
    println!("  furmi start                            run the queue and open the dashboard");
    println!("  furmi ls                               see what's queued, running and done");
    println!("  furmi --help                           every command");
    Ok(())
}

/// The default agent for a new config: the one given, the only preset on
/// $PATH, or the user's pick among several.
fn choose_agent(given: Option<String>) -> Result<Option<String>> {
    let presets = Config::default();
    if let Some(a) = given {
        if !presets.agents.contains_key(&a) {
            let known: Vec<_> = presets.agents.keys().map(String::as_str).collect();
            bail!(
                "--agent `{a}` is not a preset ({}); define it in config.toml and set defaults.agent there",
                known.join(", ")
            );
        }
        return Ok(Some(a));
    }
    let found = installed_presets();
    match found.as_slice() {
        [] => {
            println!(
                "No coding agent found yet. `furmi agents` shows how to install one; \
                 then `furmi agents use <name>` makes it the default."
            );
            Ok(None)
        }
        [one] => {
            println!("default agent: {one} (the only one found on $PATH)");
            Ok(Some(one.to_string()))
        }
        [first, ..] if !std::io::stdin().is_terminal() => {
            println!(
                "default agent: {first} (found {}; change it in config.toml)",
                found.join(", ")
            );
            Ok(Some(first.to_string()))
        }
        _ => {
            for (i, name) in found.iter().enumerate() {
                println!("  {}) {name}", i + 1);
            }
            loop {
                let answer = skill::prompt("Default agent for new tasks [1]: ")?;
                let pick = if answer.is_empty() {
                    Some(0)
                } else {
                    answer
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .or_else(|| found.iter().position(|f| *f == answer))
                };
                match pick.and_then(|i| found.get(i)) {
                    Some(name) => return Ok(Some(name.to_string())),
                    None => println!("enter a number from 1 to {}, or a name", found.len()),
                }
            }
        }
    }
}

/// Prints the listing once, or every `interval` (clearing the screen
/// between refreshes) until interrupted.
pub fn ls(home: &Home, archive: bool, schedules: bool, watch: Option<Duration>) -> Result<()> {
    let Some(interval) = watch else {
        return ls_once(home, archive, schedules);
    };
    if !schedules && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return crate::watch::run(home, archive, interval);
    }
    loop {
        print!("\x1B[2J\x1B[H"); // clear screen, cursor to top-left
        println!(
            "every {} until Ctrl-C, as of {}\n",
            humantime::format_duration(interval),
            fmt_time(now())
        );
        ls_once(home, archive, schedules)?;
        std::io::stdout().flush().ok();
        std::thread::sleep(interval);
    }
}

fn ls_once(home: &Home, archive: bool, schedules: bool) -> Result<()> {
    home.ensure_initialized()?;
    let config = Config::load(&home.config_path())?;
    if schedules {
        return ls_schedules(home, &config);
    }

    let dir = if archive {
        home.archive()
    } else {
        home.tasks()
    };
    let (tasks, warnings) = load_tasks(&dir)?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    let rows = task_rows(&config, &tasks);
    print_table(&rows);
    if tasks.is_empty() && !archive && std::io::stdout().is_terminal() {
        println!("\nNothing queued yet. Add something with: furmi add \"describe the work\"");
    }
    Ok(())
}

/// The tasks in `dir`, oldest first, plus a warning for each file that
/// couldn't be read.
pub fn load_tasks(dir: &Path) -> Result<(Vec<Task>, Vec<String>)> {
    let mut tasks = Vec::new();
    let mut warnings = Vec::new();
    for path in md_files(dir)? {
        match Task::load(&path) {
            Ok(t) => tasks.push(t),
            Err(e) => warnings.push(format!("skipping {}: {e:#}", path.display())),
        }
    }
    tasks.sort_by_key(|t| (t.scheduled_at.or(t.created_at), t.id.clone()));
    Ok((tasks, warnings))
}

/// The header row plus one row per task, as `furmi ls` shows them.
pub fn task_rows(config: &Config, tasks: &[Task]) -> Vec<Vec<String>> {
    let mut rows = vec![row([
        "ID", "STATUS", "WHEN", "REPO", "BRANCH", "AGENT", "TOKENS",
    ])];
    for t in tasks {
        let eff = t.spec.resolve(&config.defaults);
        let when = match (t.finished_at, t.started_at, t.scheduled_at) {
            (Some(at), _, _) | (None, Some(at), _) | (None, None, Some(at)) => fmt_time(at),
            _ => "asap".into(),
        };
        rows.push(vec![
            t.id.clone(),
            t.status.as_str().into(),
            when,
            contract_tilde(&eff.repo),
            t.effective_branch(config).unwrap_or_else(|| "-".into()),
            match (eff.runner(), t.model_in_use(config).0) {
                ("-", _) => "-".into(),
                (r, m) if m != "-" => format!("{r}/{m}"),
                (r, _) => r.into(),
            },
            fmt_tokens(t.tokens_in, t.tokens_out),
        ]);
    }
    rows
}

fn ls_schedules(home: &Home, config: &Config) -> Result<()> {
    let mut rows = vec![row(["ID", "ENABLED", "CRON", "NEXT", "REPO"])];
    for path in md_files(&home.schedules())? {
        let s = match Schedule::load(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: skipping {}: {e:#}", path.display());
                continue;
            }
        };
        let next = match s.next_after(Local::now()) {
            Ok(t) if s.enabled => fmt_time(t.fixed_offset()),
            Ok(_) => "-".into(),
            Err(_) => "invalid".into(),
        };
        rows.push(vec![
            s.id.clone(),
            if s.enabled { "yes" } else { "no" }.into(),
            s.cron.clone(),
            next,
            contract_tilde(&s.spec.resolve(&config.defaults).repo),
        ]);
    }
    print_table(&rows);
    Ok(())
}

#[derive(clap::Args)]
pub struct AddArgs {
    /// What the agent should do. Asked for if omitted (or read from stdin
    /// when piped).
    #[arg(conflicts_with = "prompt")]
    text: Option<String>,
    /// Repository the task runs in [default: the git repo you're in].
    #[arg(long)]
    repo: Option<PathBuf>,
    /// No project: run in a new empty folder of its own (kept until
    /// `furmi clean`), for research, drafts and the like.
    #[arg(long, conflicts_with_all = ["repo", "branch", "no_worktree", "on_finish"])]
    scratch: bool,
    /// Task id (also the file name). Derived from the prompt if omitted.
    #[arg(long)]
    id: Option<String>,
    /// The prompt, as an option instead of the positional argument.
    #[arg(long, short)]
    prompt: Option<String>,
    /// Run this shell command instead of an agent: a command task, with no
    /// prompt, agent, model or mode. Runs in the repo itself unless --worktree.
    #[arg(long, conflicts_with_all = ["text", "prompt", "agent", "model", "mode"])]
    run: Option<String>,
    #[arg(long)]
    branch: Option<String>,
    /// Run in a dedicated worktree, for a task that wouldn't by default.
    #[arg(long, conflicts_with_all = ["no_worktree", "scratch"])]
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
    /// Start time, RFC 3339 (2026-09-28T02:00:00+02:00).
    #[arg(long, conflicts_with = "delay")]
    at: Option<DateTime<FixedOffset>>,
    /// Start after a delay, e.g. 2h.
    #[arg(long = "in", value_parser = humantime::parse_duration)]
    delay: Option<Duration>,
    /// Who created the task (e.g. cli, agent).
    #[arg(long, default_value = "cli")]
    created_by: String,
    /// Save it as a draft instead of queueing it: the runner leaves it alone
    /// until `furmi queue <id>`, so it can be looked over (and edited) first.
    #[arg(long)]
    draft: bool,
    /// Start from this template (`furmi template ls`): its prompt and options
    /// are used, and anything given here replaces them.
    #[arg(long)]
    template: Option<String>,
}

/// Everything needed to queue a task, from the CLI or the dashboard.
pub struct NewTask {
    pub prompt: String,
    /// Derived from the prompt if `None`.
    pub id: Option<String>,
    pub spec: TaskSpec,
    pub scheduled_at: Option<DateTime<FixedOffset>>,
    pub created_by: String,
    /// Save as a draft: written, but not queued.
    pub draft: bool,
}

/// Validates `new` and writes its task file into the queue.
pub fn create_task(home: &Home, config: &Config, new: NewTask) -> Result<(Task, PathBuf)> {
    let prompt = new.prompt.trim().to_string();
    if prompt.is_empty() && new.spec.run.is_none() {
        bail!("the prompt is empty");
    }

    let id = match new.id {
        Some(id) => {
            validate_id(&id).map_err(anyhow::Error::msg)?;
            if id_taken(home, &id) {
                bail!("a task with id `{id}` already exists");
            }
            id
        }
        None => unique_id(
            home,
            &slugify(new.spec.ask(&prompt).lines().next().unwrap_or_default(), 48),
        ),
    };

    let mut spec = new.spec;
    // No repo: the task gets a folder of its own to work in, kept until
    // `furmi clean`. Nothing to branch or commit there.
    let scratch = spec
        .repo
        .as_os_str()
        .is_empty()
        .then(|| home.scratch().join(&id));
    if let Some(dir) = &scratch {
        std::fs::create_dir_all(home.scratch())?;
        // Fails if it exists: never someone else's folder, or its files.
        std::fs::create_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
        spec.repo = dir.clone();
        spec.worktree = Some(false);
        spec.branch = None;
        spec.on_finish = Some(OnFinish::None);
    }
    let made = (|| {
        let repo = std::path::absolute(expand_tilde(&spec.repo))?;
        spec.repo = PathBuf::from(contract_tilde(&repo));
        let mut task = Task::new(id, spec, prompt);
        task.created_by = Some(new.created_by);
        task.scheduled_at = new.scheduled_at;
        if new.draft {
            task.status = Status::Draft;
        }

        let problems = task.problems(config, None);
        if !problems.is_empty() {
            bail!("invalid task:\n  - {}", problems.join("\n  - "));
        }
        // A scratch folder is no clone of anything, even inside a repo.
        if scratch.is_none() {
            task.fill_remote();
        }
        let path = home.tasks().join(format!("{}.md", task.id));
        task.create(&path)?;
        Ok((task, path))
    })();
    if made.is_err()
        && let Some(dir) = scratch
    {
        let _ = std::fs::remove_dir(dir);
    }
    made
}

pub fn add(home: &Home, args: AddArgs) -> Result<()> {
    home.ensure_initialized()?;
    let config = Config::load(&home.config_path())?;

    let template = args
        .template
        .as_deref()
        .map(|id| Template::find(home, id))
        .transpose()?;
    let given = args.text.or(args.prompt);
    let prompt = match (&args.run, &template) {
        (Some(_), _) => String::new(),
        // The template's own prompt, unless one was given to replace it.
        (None, Some(t)) => given.unwrap_or_else(|| t.prompt.clone()),
        (None, None) => read_prompt(given)?,
    };
    let base = template.as_ref().map(|t| t.spec.clone());
    let repo = match args.repo {
        _ if args.scratch => PathBuf::new(),
        Some(repo) => repo,
        None => match template.as_ref().and_then(|t| t.repo()) {
            Some(repo) => repo.to_path_buf(),
            None => current_repo()?,
        },
    };
    let base = base.as_ref();
    let command = args.run.is_some();
    let spec = TaskSpec {
        repo,
        branch: args.branch.or_else(|| base.and_then(|s| s.branch.clone())),
        worktree: worktree_choice(args.worktree, args.no_worktree)
            .or_else(|| base.and_then(|s| s.worktree)),
        // A command has no agent, model or mode, whatever the template said.
        agent: args
            .agent
            .or_else(|| base.and_then(|s| s.agent.clone()).filter(|_| !command)),
        model: args
            .model
            .or_else(|| base.and_then(|s| s.model.clone()).filter(|_| !command)),
        mode: args
            .mode
            .or_else(|| base.and_then(|s| s.mode).filter(|_| !command)),
        on_finish: args.on_finish.or_else(|| base.and_then(|s| s.on_finish)),
        retries: args.retries.or_else(|| base.and_then(|s| s.retries)),
        retry_delay: args
            .retry_delay
            .or_else(|| base.and_then(|s| s.retry_delay)),
        timeout: args.timeout.or_else(|| base.and_then(|s| s.timeout)),
        run: args.run.or_else(|| base.and_then(|s| s.run.clone())),
    };
    let scheduled_at = match (args.at, args.delay) {
        (Some(at), _) => Some(at),
        (None, Some(delay)) => Some(now() + delay),
        (None, None) => None,
    };
    let (task, path) = create_task(
        home,
        &config,
        NewTask {
            prompt,
            id: args.id,
            spec,
            scheduled_at,
            created_by: args.created_by,
            draft: args.draft,
        },
    )?;

    if std::io::stdout().is_terminal() {
        print_queued(home, &config, &task);
    } else {
        // Scripts and agents read the path of the new file.
        println!("{}", path.display());
    }
    Ok(())
}

/// What `--worktree` and `--no-worktree` ask for, if either was given.
pub(crate) fn worktree_choice(worktree: bool, no_worktree: bool) -> Option<bool> {
    (worktree || no_worktree).then_some(worktree)
}

/// The given prompt, else the one piped on stdin, else (on a terminal) the
/// one the user types.
pub(crate) fn read_prompt(given: Option<String>) -> Result<String> {
    if let Some(prompt) = given {
        return Ok(prompt);
    }
    if std::io::stdin().is_terminal() {
        let prompt = skill::prompt("What should the agent do? ")?;
        if prompt.is_empty() {
            bail!("the prompt is empty");
        }
        return Ok(prompt);
    }
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

/// The git repo the user is standing in, for `furmi add` without `--repo`.
pub(crate) fn current_repo() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("finding the current folder")?;
    crate::git::repo_root(&cwd).map_err(|_| {
        anyhow::anyhow!(
            "{} isn't inside a git repository: run `furmi add` from your project's folder, \
             or say where with --repo <path> (or --scratch for a task with no project)",
            contract_tilde(&cwd)
        )
    })
}

/// Plain-language confirmation for a person at a terminal, and what to do next.
fn print_queued(home: &Home, config: &Config, task: &Task) {
    let eff = task.spec.resolve(&config.defaults);
    let draft = task.status == Status::Draft;
    println!(
        "{} `{}`",
        if draft { "Saved a draft," } else { "Queued" },
        task.id
    );
    println!("  repo       {}", contract_tilde(&eff.repo));
    match &eff.run {
        Some(run) => println!("  command    {run}"),
        None => println!("  agent      {}", eff.runner()),
    }
    match (task.scheduled_at, draft) {
        (Some(at), _) => println!("  starts     {}", fmt_time(at)),
        (None, true) => println!("  starts     when you queue it"),
        (None, false) => println!("  starts     as soon as the runner picks it up"),
    }
    println!("  when done  {}", describe_on_finish(eff.on_finish));
    println!();
    if draft {
        println!(
            "It won't run until you queue it: look it over, then `furmi queue {}` (or Queue \
             it in the dashboard).",
            task.id
        );
        return;
    }
    if home.runner_alive() {
        println!(
            "The runner is up, so it will start on its own. Follow along with `furmi ls --watch`."
        );
    } else {
        println!("Nothing runs it yet: start the runner (and the dashboard) with `furmi start`.");
    }
}

fn describe_on_finish(on_finish: OnFinish) -> &'static str {
    match on_finish {
        OnFinish::None => "leave the changes for you to review",
        OnFinish::Commit => "commit the changes",
        OnFinish::Push => "commit and push the changes",
        OnFinish::Pr => "commit, push and open a pull request",
    }
}

fn id_taken(home: &Home, id: &str) -> bool {
    let file = format!("{id}.md");
    home.tasks().join(&file).exists()
        || home.archive().join(&file).exists()
        // A scratch folder outlives its task until `furmi clean`.
        || home.scratch().join(id).exists()
}

pub fn unique_id(home: &Home, base: &str) -> String {
    (1..)
        .map(|n| {
            if n == 1 {
                base.to_string()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|id| !id_taken(home, id))
        .expect("infinite iterator")
}

/// Validates the given files, or every task and schedule if none are given.
/// Prints one line per file so agents can parse the result.
pub fn validate(home: &Home, files: Vec<PathBuf>) -> Result<ExitCode> {
    let config = Config::load(&home.config_path())?;
    let files = if files.is_empty() {
        home.ensure_initialized()?;
        let mut all = md_files(&home.tasks())?;
        all.extend(md_files(&home.schedules())?);
        all.extend(md_files(&home.templates()).unwrap_or_default());
        all
    } else {
        files
    };

    let mut failed = 0;
    for path in &files {
        let problems = match validate_file(&config, path) {
            Ok(p) => p,
            Err(e) => vec![format!("{e:#}")],
        };
        if problems.is_empty() {
            println!("ok     {}", path.display());
        } else {
            failed += 1;
            println!("error  {}", path.display());
            for p in problems {
                println!("       - {p}");
            }
        }
    }
    println!("{} file(s) checked, {failed} with errors", files.len());
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn validate_file(config: &Config, path: &Path) -> Result<Vec<String>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if path
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|d| d == "templates")
    {
        return template::validate_file(path);
    }
    Ok(if frontmatter::has_key(&text, "cron") {
        Schedule::parse(&text)?.problems(config, Some(path))
    } else {
        Task::parse(&text)?.problems(config, Some(path))
    })
}

pub fn md_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    Ok(files)
}

pub fn fmt_time(t: DateTime<FixedOffset>) -> String {
    t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

fn row<const N: usize>(cells: [&str; N]) -> Vec<String> {
    cells.iter().map(|s| s.to_string()).collect()
}

/// Aligned table lines, header first, trailing spaces trimmed.
pub fn table_lines(rows: &[Vec<String>]) -> Vec<String> {
    let cols = rows[0].len();
    let widths: Vec<usize> = (0..cols)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|r| {
            let line: Vec<String> = r
                .iter()
                .zip(&widths)
                .map(|(cell, w)| format!("{cell:<w$}"))
                .collect();
            line.join("  ").trim_end().to_string()
        })
        .collect()
}

fn print_table(rows: &[Vec<String>]) {
    if rows.len() == 1 {
        println!("(none)");
        return;
    }
    for line in table_lines(rows) {
        println!("{line}");
    }
}
