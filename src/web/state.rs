//! What `/api/state` serves: the counts, the queue, running and archived
//! rows, the schedules, the files that wouldn't parse, the runner, and what
//! the new-task form offers.

use std::cmp::Reverse;

use anyhow::Result;
use serde::Serialize;

use crate::cmd::md_files;
use crate::config::{Config, on_path};
use crate::edit;
use crate::home::{Home, contract_tilde};
use crate::run::RunnerState;
use crate::schedule::Schedule;
use crate::task::{Status, Task, now};

#[derive(Serialize)]
struct Row {
    id: String,
    status: &'static str,
    when: String,
    /// The instant behind `when`, RFC 3339 (and `None` when there is none:
    /// "as soon as possible"). The page sorts and time-filters on this and
    /// ticks the "running for" clock from it, since the printed form has
    /// minute precision and its own idea of "asap".
    ///
    /// Token usage deliberately isn't here: it only exists for completed
    /// headless runs of agents that print JSON, so a column of it would be
    /// mostly dashes and easy to mistake for accounting. `furmi ls` and the task
    /// files still have it.
    when_at: Option<String>,
    repo: String,
    branch: String,
    agent: String,
    /// The model this task is about: what it actually ran with once it has
    /// run (its own `model_used`, from the agent's output), else the model it
    /// pins, else the one its agent is configured with. `"-"` when nothing
    /// says, e.g. an agent on its own default. Its own field rather than glued
    /// onto the agent because real model names are long (`hf.co/unsloth/\
    /// Qwen3.6-35B-A3B-GGUF:UD-Q4_K_XL`).
    model: String,
    /// Which of those `model` came from, so the page can say where it got it:
    /// `used` (the agent reported it), `task` (the task pins it), `agent` (the
    /// agent's configured default) or `none`.
    model_source: &'static str,
    /// What the task asks for, on one line and short: enough to search it and
    /// to tell two tasks in the same repo apart without opening either.
    summary: String,
    /// Why a failed (or otherwise unfinished) task ended up that way.
    error: Option<String>,
}

#[derive(Serialize)]
struct ScheduleRow {
    id: String,
    /// The first line of what it asks the agent to do.
    summary: String,
    enabled: bool,
    cron: String,
    next: String,
    repo: String,
}

/// A task, schedule or config file that couldn't be read.
#[derive(Serialize)]
struct InvalidRow {
    file: String,
    error: String,
}

#[derive(Serialize)]
struct RunnerRow {
    alive: bool,
    last_tick: Option<String>,
}

#[derive(Serialize, Default)]
struct Counts {
    pending: usize,
    running: usize,
    done: usize,
    failed: usize,
    cancelled: usize,
    needs_review: usize,
    schedules: usize,
    invalid: usize,
}

/// What the "New task" form offers and pre-selects.
#[derive(Serialize)]
struct FormInfo {
    /// Repos to suggest, most relevant first.
    repos: Vec<String>,
    agents: Vec<String>,
    /// The default agent, if one is set.
    agent: Option<String>,
    on_finish: &'static str,
}

#[derive(Serialize)]
struct State {
    home: String,
    generated_at: String,
    counts: Counts,
    queue: Vec<Row>,
    running: Vec<Row>,
    archive: Vec<Row>,
    schedules: Vec<ScheduleRow>,
    invalid: Vec<InvalidRow>,
    runner: RunnerRow,
    form: FormInfo,
}

/// Loads every file in `paths`, setting aside the ones that don't parse
/// instead of failing the whole dashboard over one bad file.
fn load_all<T>(
    paths: Vec<std::path::PathBuf>,
    load: impl Fn(&std::path::Path) -> Result<T>,
    invalid: &mut Vec<InvalidRow>,
) -> Vec<T> {
    let mut ok = Vec::new();
    for path in paths {
        match load(&path) {
            Ok(v) => ok.push(v),
            Err(e) => invalid.push(InvalidRow {
                file: contract_tilde(&path),
                error: format!("{e:#}"),
            }),
        }
    }
    ok
}

pub(super) fn state_json(home: &Home, cwd_repo: Option<&str>) -> Result<String> {
    let mut invalid = Vec::new();
    let config = Config::load(&home.config_path()).unwrap_or_else(|e| {
        invalid.push(InvalidRow {
            file: contract_tilde(&home.config_path()),
            error: format!("{e:#} (showing built-in defaults)"),
        });
        Config::default()
    });
    let mut counts = Counts::default();

    let (mut running_tasks, mut queue_tasks): (Vec<_>, Vec<_>) =
        load_all(md_files(&home.tasks())?, Task::load, &mut invalid)
            .into_iter()
            .partition(|t| t.status == Status::Running);
    queue_tasks.sort_by_key(|t| (t.scheduled_at.or(t.created_at), t.id.clone()));
    running_tasks.sort_by_key(|t| (t.started_at, t.id.clone()));
    counts.pending = queue_tasks.len();
    counts.running = running_tasks.len();

    let mut archive_tasks = load_all(md_files(&home.archive())?, Task::load, &mut invalid);
    for t in &archive_tasks {
        match t.status {
            Status::Done => counts.done += 1,
            Status::Failed => counts.failed += 1,
            Status::Cancelled => counts.cancelled += 1,
            Status::NeedsReview => counts.needs_review += 1,
            Status::Pending | Status::Running => {}
        }
    }
    archive_tasks.sort_by_key(|t| Reverse(t.finished_at));
    let form = form_info(
        &config,
        cwd_repo,
        &[&running_tasks, &queue_tasks, &archive_tasks],
    );

    let schedules: Vec<ScheduleRow> =
        load_all(md_files(&home.schedules())?, Schedule::load, &mut invalid)
            .into_iter()
            .map(|s| {
                let next = match s.next_after(chrono::Local::now()) {
                    Ok(t) if s.enabled => fmt_time(t.fixed_offset()),
                    Ok(_) => "-".into(),
                    Err(_) => "invalid".into(),
                };
                ScheduleRow {
                    id: s.id.clone(),
                    summary: s.prompt.lines().next().unwrap_or_default().to_string(),
                    enabled: s.enabled,
                    cron: s.cron.clone(),
                    next,
                    repo: contract_tilde(&s.spec.resolve(&config.defaults).repo),
                }
            })
            .collect();
    counts.schedules = schedules.len();
    counts.invalid = invalid.len();

    let state = State {
        home: contract_tilde(home.root()),
        generated_at: fmt_time(now()),
        counts,
        queue: queue_tasks.iter().map(|t| to_row(t, &config)).collect(),
        running: running_tasks.iter().map(|t| to_row(t, &config)).collect(),
        archive: archive_tasks.iter().map(|t| to_row(t, &config)).collect(),
        schedules,
        invalid,
        runner: RunnerRow {
            alive: home.runner_alive(),
            last_tick: RunnerState::load(home)
                .and_then(|s| s.last_tick_at)
                .map(fmt_time),
        },
        form,
    };
    Ok(serde_json::to_string(&state)?)
}

/// What this node can host, for a hub placing a task: the repos it has a clone
/// of, keyed by the remote identity (`github.com/me/app`) rather than the
/// local path, and the agents that can run here. Repos without an `origin`
/// have no identity another machine could ask for, so they're left out.
pub(super) fn repos_json(home: &Home, cwd_repo: Option<&str>) -> Result<String> {
    let hosts = hostable(home, cwd_repo)?;
    let repos: Vec<_> = hosts
        .repos
        .iter()
        .map(|(remote, path)| serde_json::json!({ "remote": remote, "path": path }))
        .collect();
    Ok(
        serde_json::json!({ "repos": repos, "agents": hosts.agents, "agent": hosts.agent })
            .to_string(),
    )
}

/// What a node can run: its repos by remote identity, and its usable agents.
pub(super) struct Hostable {
    /// `(remote, path)`, one per remote.
    pub repos: Vec<(String, String)>,
    pub agents: Vec<String>,
    pub agent: Option<String>,
}

pub(super) fn hostable(home: &Home, cwd_repo: Option<&str>) -> Result<Hostable> {
    let config = Config::load(&home.config_path())?;
    let mut ignored = Vec::new();
    let running_and_queue = load_all(md_files(&home.tasks())?, Task::load, &mut ignored);
    let archive = load_all(md_files(&home.archive())?, Task::load, &mut ignored);
    let form = form_info(&config, cwd_repo, &[&running_and_queue, &archive]);
    let mut seen = std::collections::BTreeSet::new();
    let repos = form
        .repos
        .iter()
        .filter_map(|path| {
            // The checkout's root, not a subfolder a task happened to name.
            let root = crate::git::repo_key(&crate::home::expand_tilde(path.as_ref()));
            let remote = crate::git::remote_id(&root)?;
            seen.insert(remote.clone())
                .then(|| (remote, contract_tilde(&root)))
        })
        .collect();
    Ok(Hostable {
        repos,
        agents: form.agents,
        agent: form.agent,
    })
}

/// This machine's clone of the repo whose remote identity is `remote`.
pub(super) fn path_of_remote(home: &Home, cwd_repo: Option<&str>, remote: &str) -> Result<String> {
    hostable(home, cwd_repo)?
        .repos
        .into_iter()
        .find(|(r, _)| r == remote)
        .map(|(_, path)| path)
        .ok_or_else(|| anyhow::anyhow!("no clone of {remote} on this machine"))
}

/// Repos from the folder `furmi web` started in, then from recent tasks (newest
/// first, each once), plus the configured agents and finish action.
fn form_info(config: &Config, cwd_repo: Option<&str>, tasks: &[&Vec<Task>]) -> FormInfo {
    let mut recent: Vec<&Task> = tasks.iter().flat_map(|t| t.iter()).collect();
    recent.sort_by_key(|t| Reverse(t.created_at));
    let mut repos: Vec<String> = Vec::new();
    for repo in cwd_repo.map(str::to_string).into_iter().chain(
        recent
            .iter()
            .map(|t| contract_tilde(&t.spec.resolve(&config.defaults).repo)),
    ) {
        if !repos.contains(&repo) {
            repos.push(repo);
        }
    }
    // Only agents that can actually run here (the default always counts), so
    // the form doesn't offer a choice that would just fail.
    let agents = config
        .agents
        .iter()
        .filter(|(name, agent)| {
            config.defaults.agent.as_deref() == Some(name.as_str())
                || agent.headless.first().is_some_and(|p| on_path(p))
        })
        .map(|(name, _)| name.clone())
        .collect();
    FormInfo {
        repos,
        agents,
        agent: config.defaults.agent.clone(),
        on_finish: edit::on_finish_str(config.defaults.on_finish),
    }
}

fn to_row(t: &Task, config: &Config) -> Row {
    let eff = t.spec.resolve(&config.defaults);
    let at = match (t.finished_at, t.started_at, t.scheduled_at) {
        (Some(at), _, _) | (None, Some(at), _) | (None, None, Some(at)) => Some(at),
        (None, None, None) => None,
    };
    let (model, model_source) = t.model_in_use(config);
    Row {
        id: t.id.clone(),
        status: t.status.as_str(),
        when: at.map(fmt_time).unwrap_or_else(|| "asap".into()),
        when_at: at.map(|at| at.to_rfc3339()),
        repo: contract_tilde(&eff.repo),
        branch: t.effective_branch(config).unwrap_or_else(|| "-".into()),
        agent: eff.agent.clone().unwrap_or_else(|| "-".into()),
        summary: summary_of(&t.prompt),
        model,
        model_source,
        error: t.error.clone().filter(|e| !e.trim().is_empty()),
    }
}

/// A task's prompt on one line, cut short: the first non-empty line of it,
/// which for a task written like a message is the ask.
fn summary_of(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let line = line.trim_start_matches(['#', '-', '*', '>', ' ']);
    let mut summary: String = line.chars().take(120).collect();
    if summary.len() < line.len() {
        summary = summary.trim_end().to_string();
        summary.push('…');
    }
    summary
}

pub(super) fn fmt_time(t: chrono::DateTime<chrono::FixedOffset>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::TaskSpec;

    #[test]
    fn a_failed_tasks_reason_reaches_its_row() {
        let spec = TaskSpec {
            repo: "/tmp".into(),
            branch: None,
            worktree: None,
            agent: Some("claude".into()),
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
        };
        let mut task = Task::new("t".into(), spec, "do it".into());
        let config = Config::default();
        assert_eq!(to_row(&task, &config).error, None);

        task.status = Status::Failed;
        task.error = Some("exit code 1".into());
        assert_eq!(to_row(&task, &config).error.as_deref(), Some("exit code 1"));

        // A provider/model failure reaches the dashboard with its reason.
        task.error = Some("the agent's provider rejected the run: model is not available".into());
        assert!(
            to_row(&task, &config)
                .error
                .unwrap()
                .contains("provider rejected the run")
        );

        task.error = Some("  ".into());
        assert_eq!(to_row(&task, &config).error, None);
    }

    #[test]
    fn a_row_carries_the_time_the_page_sorts_on() {
        let spec = TaskSpec {
            repo: "/tmp".into(),
            branch: None,
            worktree: None,
            agent: None,
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
        };
        let mut task = Task::new("t".into(), spec, "do it".into());
        let config = Config::default();

        // Waiting with no start time: nothing to sort on or count down from.
        let row = to_row(&task, &config);
        assert_eq!(row.when, "asap");
        assert_eq!(row.when_at, None);
        assert_eq!(row.agent, "-");
        assert_eq!(row.model, "-");

        // Running: a real timestamp, in a form the page can parse, and the
        // agent and model each on their own (the page shows them side by side).
        task.status = Status::Running;
        task.started_at = Some(now());
        task.spec.agent = Some("pi".into());
        task.spec.model = Some("hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-Q4_K_XL".into());
        let row = to_row(&task, &config);
        assert_eq!(row.agent, "pi");
        assert_eq!(row.model, "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-Q4_K_XL");
        let at = row.when_at.expect("a running task knows when it started");
        assert!(chrono::DateTime::parse_from_rfc3339(&at).is_ok(), "{at}");
    }

    #[test]
    fn the_model_column_names_what_ran_and_says_where_it_got_the_name() {
        let spec = TaskSpec {
            repo: "/tmp".into(),
            branch: None,
            worktree: None,
            agent: None,
            model: None,
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
        };
        let mut task = Task::new("t".into(), spec, "do it".into());
        let mut config = Config::default();

        // Nothing anywhere: nothing to name.
        assert_eq!(to_row(&task, &config).model, "-");
        assert_eq!(to_row(&task, &config).model_source, "none");

        // The agent's configured model is the answer when the task pins none.
        config.agents.insert(
            "pi".into(),
            crate::config::Agent {
                headless: Vec::new(),
                interactive: Vec::new(),
                model_args: Vec::new(),
                model: Some("agent/default".into()),
            },
        );
        config.defaults.agent = Some("pi".into());
        let row = to_row(&task, &config);
        assert_eq!(row.model, "agent/default");
        assert_eq!(row.model_source, "agent");

        // A task that pins one says so, overriding the agent's.
        task.spec.model = Some("pinned/model".into());
        let row = to_row(&task, &config);
        assert_eq!(row.model, "pinned/model");
        assert_eq!(row.model_source, "task");

        // Still waiting: what it *will* run with is the pinned model, even if
        // an earlier attempt reported its own.
        task.model_used = Some("reported/model".into());
        assert_eq!(to_row(&task, &config).model, "pinned/model");

        // Finished: the agent's own answer is the truth, model_used wins.
        task.status = Status::Done;
        task.finished_at = Some(now());
        let row = to_row(&task, &config);
        assert_eq!(row.model, "reported/model");
        assert_eq!(row.model_source, "used");

        // A finished task with no report of its own falls back to the pin.
        task.model_used = None;
        assert_eq!(to_row(&task, &config).model, "pinned/model");
    }
}
