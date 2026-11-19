//! The JSON handlers behind the routes: the task actions, the new-task, edit
//! and schedule forms, the agents and doctor views, and a task's own view.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::state::fmt_time;
use super::{Reply, json_reply};

use crate::cmd::{NewTask, create_task};
use crate::config::Config;
use crate::control;
use crate::edit::{self, ScheduleEdit, Start, TaskEdit, on_finish_str};
use crate::home::{Home, contract_tilde};
use crate::spec::{OnFinish, TaskSpec};
use crate::task::Task;
use crate::template::{self, Template};

#[derive(Deserialize, Default)]
struct ActionBody {
    reason: Option<String>,
}

/// Runs one of the `ACTIONS` against a task, the same way `furmi <action> <id>`
/// would. `body` is the POST body, parsed for `fail`'s optional `reason`.
pub(super) fn perform_action(home: &Home, id: &str, action: &str, body: &str) -> Result<()> {
    match action {
        "done" => control::done(home, Some(id.to_string())),
        "fail" => {
            let reason = serde_json::from_str::<ActionBody>(body)
                .ok()
                .and_then(|b| b.reason)
                .filter(|r| !r.trim().is_empty());
            control::fail(home, Some(id.to_string()), reason)
        }
        "cancel" => control::cancel(home, Some(id.to_string())),
        "retry" => control::retry(home, id),
        "queue" => {
            #[derive(Deserialize, Default)]
            struct QueueBody {
                start: Option<String>,
            }
            let form: QueueBody = serde_json::from_str(body).unwrap_or_default();
            control::queue(home, id, edit::parse_start(form.start.as_deref())?)
        }
        _ => unreachable!("route only matches ACTIONS"),
    }
}

#[derive(Deserialize, Default)]
struct NameBody {
    /// What to call the copy; made up from the task if empty.
    #[serde(default)]
    id: Option<String>,
}

fn chosen_name(body: &str) -> Option<String> {
    serde_json::from_str::<NameBody>(body)
        .ok()
        .and_then(|b| b.id)
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// "Duplicate as new draft": copies a task into a draft. Returns its id.
pub(super) fn perform_duplicate(home: &Home, id: &str, body: &str) -> Result<String> {
    control::duplicate(home, id, chosen_name(body))
}

/// "Save as template". Returns the template's id.
pub(super) fn perform_save_template(home: &Home, id: &str, body: &str) -> Result<String> {
    template::save_from_task(home, id, chosen_name(body))
}

/// What the new-task form starts from when a template is used.
pub(super) fn template_detail(home: &Home, id: &str) -> Result<serde_json::Value> {
    let config = Config::load(&home.config_path())?;
    let t = Template::find(home, id)?;
    let eff = t.spec.resolve(&config.defaults);
    Ok(serde_json::json!({
        "id": t.id,
        "description": t.description,
        // Carried into the first draft of the new task's form.
        "title": t.spec.display_title(),
        "prompt": t.spec.ask(&t.prompt),
        "command": t.spec.run.is_some(),
        "repo": t.repo().map(contract_tilde).unwrap_or_default(),
        "on_finish": on_finish_str(eff.on_finish),
        "agent": eff.agent,
    }))
}

/// The template form, for creating and editing alike.
#[derive(Deserialize)]
struct TemplateBody {
    #[serde(default)]
    description: String,
    prompt: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
}

fn template_form(body: &str) -> Result<template::TemplateEdit> {
    let form: TemplateBody = serde_json::from_str(body).context("couldn't read the form")?;
    Ok(template::TemplateEdit {
        description: form.description,
        prompt: form.prompt,
        repo: form.repo,
        on_finish: form.on_finish,
        agent: form.agent,
    })
}

pub(super) fn perform_create_template(home: &Home, body: &str) -> Result<String> {
    template::create(home, template_form(body)?)
}

pub(super) fn perform_edit_template(home: &Home, id: &str, body: &str) -> Result<()> {
    template::update(home, id, template_form(body)?)
}

pub(super) fn perform_delete_template(home: &Home, id: &str) -> Result<()> {
    template::delete(home, id)
}

/// The "New task" form.
#[derive(Deserialize)]
struct NewTaskBody {
    /// One line saying what the task is about; empty means the form left it out
    /// and the listing shows the prompt's first line instead.
    #[serde(default)]
    title: Option<String>,
    prompt: String,
    #[serde(default)]
    repo: String,
    /// Instead of `repo`: the repo's remote identity (`github.com/me/app`),
    /// for a task another machine placed here. Resolved to this machine's
    /// clone of it.
    #[serde(default)]
    remote: Option<String>,
    /// No project: a folder of its own to work in.
    #[serde(default)]
    scratch: bool,
    /// `asap` (or empty), or a delay like `1h`.
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
    /// Save it for review instead of queueing it.
    #[serde(default)]
    draft: bool,
    /// Start from this template's options (the prompt is the form's own).
    #[serde(default)]
    template: Option<String>,
    /// Pin a model for this task; empty or absent means the agent's own.
    #[serde(default)]
    model: Option<String>,
    /// The form's "More options": what the branch starts from, a command to
    /// prepare the workspace, tasks to wait for (ids, separated by commas or
    /// spaces) and the place in the queue.
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    setup: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
}

/// Queues the task described by the form, as `furmi add` would. Returns its id.
pub(super) fn perform_create(home: &Home, cwd_repo: Option<&str>, body: &str) -> Result<String> {
    let form: NewTaskBody = serde_json::from_str(body).context("couldn't read the form")?;
    let remote = form
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    let resolved;
    let repo = match (form.repo.trim(), remote) {
        _ if form.scratch => "",
        ("", None) => anyhow::bail!("choose the repository the agent should work in"),
        ("", Some(remote)) => {
            resolved = super::state::path_of_remote(home, cwd_repo, remote)?;
            resolved.as_str()
        }
        (repo, _) => repo,
    };
    // A new task has nothing to "keep": no start time means as soon as possible.
    let scheduled_at = match edit::parse_start(form.start.as_deref())? {
        Start::At(at) => Some(at),
        Start::Keep | Start::Asap => None,
    };
    let config = Config::load(&home.config_path())?;
    // A template brings the options the form doesn't show (model, branch...);
    // what the form does show replaces its own.
    let base = match form
        .template
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(id) => Some(Template::find(home, id)?.spec),
        None => None,
    };
    let command = base.as_ref().is_some_and(|b| b.run.is_some());
    let mut spec = base.unwrap_or_else(|| edit::blank_spec(repo.into()));
    spec.repo = repo.into();
    if command {
        // A command has no agent, model or mode, whatever the template said.
        spec.agent = None;
        spec.model = None;
        spec.mode = None;
    }
    if let Some(agent) = form.agent.filter(|a| !a.trim().is_empty() && !command) {
        spec.agent = Some(agent);
    }
    if let Some(model) = form
        .model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty() && !command)
    {
        spec.model = Some(model);
    }
    if form.on_finish.is_some() {
        spec.on_finish = form.on_finish;
    }
    if let Some(title) = TaskSpec::clean_title(form.title.as_deref()) {
        spec.title = Some(title);
    } else if form.title.is_some() {
        // An emptied box clears whatever the template carried.
        spec.title = None;
    }
    let filled =
        |text: Option<String>| text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if let Some(base) = filled(form.base) {
        spec.base = Some(base);
    }
    if let Some(setup) = filled(form.setup) {
        spec.setup = Some(setup);
    }
    if let Some(after) = filled(form.after) {
        spec.after = after
            .split([',', ' ', '\n'])
            .map(str::trim)
            .filter(|id| !id.is_empty())
            // A row of another machine reads `id@node`; the wait is local.
            .map(|id| id.split('@').next().unwrap_or(id).to_string())
            .collect();
    }
    if form.priority.is_some_and(|p| p != 0) {
        spec.priority = form.priority;
    }
    // The form's one text box holds a command task's command.
    let mut prompt = form.prompt;
    if command {
        spec.run = Some(std::mem::take(&mut prompt).trim().to_string());
    }
    let created = create_task(
        home,
        &config,
        NewTask {
            prompt,
            id: None,
            spec,
            scheduled_at,
            created_by: "web".into(),
            draft: form.draft,
        },
    );
    match created {
        Ok((task, _)) => Ok(task.id),
        // The form shows one plain sentence, not the CLI's bulleted list.
        Err(e) => {
            let message = format!("{e:#}");
            match message.strip_prefix("invalid task:\n  - ") {
                Some(problems) => anyhow::bail!("{}", problems.replace("\n  - ", "; ")),
                None => Err(e),
            }
        }
    }
}

/// 200 with the object, or 404 with why there isn't one.
pub(super) fn detail_reply(detail: Result<serde_json::Value>) -> Reply {
    match detail {
        Ok(v) => json_reply(200, v.to_string()),
        Err(e) => json_reply(404, json_err(&e)),
    }
}

/// `{"ok":true}`, or 400 with what went wrong.
pub(super) fn mutation_reply(result: Result<()>) -> Reply {
    match result {
        Ok(()) => json_reply(200, "{\"ok\":true}".into()),
        Err(e) => json_reply(400, json_err(&e)),
    }
}

/// The edit form for a task that hasn't started.
#[derive(Deserialize)]
struct EditTaskBody {
    /// `None` leaves the title as it is; an empty string clears it.
    #[serde(default)]
    title: Option<String>,
    prompt: String,
    repo: String,
    /// `keep` (or empty), `asap`, or a delay like `1h`.
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
    /// The form always says what this field holds: empty means "the agent's
    /// own", `None` (a form without the field) leaves it as it is.
    #[serde(default)]
    model: Option<String>,
}

pub(super) fn perform_edit_task(home: &Home, id: &str, body: &str) -> Result<()> {
    let form: EditTaskBody = serde_json::from_str(body).context("couldn't read the form")?;
    edit::edit_task(
        home,
        id,
        TaskEdit {
            title: form.title,
            prompt: form.prompt,
            repo: form.repo,
            start: edit::parse_start(form.start.as_deref())?,
            on_finish: form.on_finish,
            agent: form.agent,
            model: form.model,
        },
    )
}

/// The schedule form, for creating and editing alike.
#[derive(Deserialize)]
struct ScheduleBody {
    #[serde(default)]
    title: Option<String>,
    prompt: String,
    #[serde(default)]
    repo: String,
    /// Instead of `repo`: the project by git remote, for another machine's form.
    #[serde(default)]
    remote: Option<String>,
    cron: String,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
    /// Empty clears it (the agent's own model then applies); absent leaves it.
    #[serde(default)]
    model: Option<String>,
}

fn schedule_form(
    home: &Home,
    cwd_repo: Option<&str>,
    body: &str,
    allow_remote: bool,
) -> Result<ScheduleEdit> {
    let form: ScheduleBody = serde_json::from_str(body).context("couldn't read the form")?;
    if form.remote.is_some() && !allow_remote {
        anyhow::bail!("a schedule's project is changed by its folder, not by git remote");
    }
    let repo = match (form.repo.trim(), form.remote.as_deref().map(str::trim)) {
        ("", Some(remote)) if !remote.is_empty() => {
            super::state::path_of_remote(home, cwd_repo, remote)?
        }
        (repo, _) => repo.to_string(),
    };
    Ok(ScheduleEdit {
        title: form.title,
        prompt: form.prompt,
        repo,
        cron: form.cron,
        on_finish: form.on_finish,
        agent: form.agent,
        model: form.model,
    })
}

pub(super) fn perform_create_schedule(
    home: &Home,
    cwd_repo: Option<&str>,
    body: &str,
) -> Result<String> {
    edit::create_schedule(home, schedule_form(home, cwd_repo, body, true)?)
}

pub(super) fn perform_edit_schedule(home: &Home, id: &str, body: &str) -> Result<()> {
    edit::edit_schedule(home, id, schedule_form(home, None, body, false)?)
}

#[derive(Deserialize)]
struct DefaultAgentBody {
    agent: String,
}

pub(super) fn perform_set_default_agent(home: &Home, body: &str) -> Result<()> {
    let form: DefaultAgentBody = serde_json::from_str(body).context("couldn't read the form")?;
    home.ensure_initialized()?;
    crate::config::set_default_agent(&home.config_path(), &form.agent)
}

#[derive(Deserialize)]
struct LimitsBody {
    max_parallel: usize,
    max_parallel_per_repo: usize,
    poll_interval: String,
    /// Tokens a day before the queue is held; 0 for no budget. Absent (an
    /// older page) leaves it as it is.
    #[serde(default)]
    daily_token_budget: Option<u64>,
}

#[derive(Deserialize)]
struct AllowBody {
    agent: String,
    level: String,
}

/// "How far may it go?" in the setup view.
pub(super) fn perform_allow_agent(home: &Home, body: &str) -> Result<()> {
    let form: AllowBody = serde_json::from_str(body).context("couldn't read the form")?;
    home.ensure_initialized()?;
    crate::config::set_unattended(
        &home.config_path(),
        &form.agent,
        crate::config::Leash::parse(&form.level)?,
    )
}

#[derive(Deserialize)]
struct NotifyBody {
    #[serde(default)]
    on: Vec<String>,
    #[serde(default)]
    ntfy_url: String,
    #[serde(default)]
    ntfy_topic: String,
    #[serde(default)]
    telegram_chat_id: String,
}

pub(super) fn perform_set_notify(home: &Home, body: &str) -> Result<()> {
    let form: NotifyBody = serde_json::from_str(body).context("couldn't read the form")?;
    home.ensure_initialized()?;
    crate::config::set_notify(
        &home.config_path(),
        &crate::config::NotifyForm {
            on: &form.on,
            ntfy_url: &form.ntfy_url,
            ntfy_topic: &form.ntfy_topic,
            telegram_chat_id: &form.telegram_chat_id,
        },
    )
}

/// Sends a message to every configured service, whatever `on` says, so the
/// settings can be tried without waiting for a task to end.
pub(super) fn perform_test_notify(home: &Home) -> Result<()> {
    let config = Config::load(&home.config_path())?;
    if config.notify.ntfy.is_none() && config.notify.telegram.is_none() {
        anyhow::bail!("no service is set up yet: fill in ntfy or Telegram and save first");
    }
    let problems = crate::notify::problems(&config.notify);
    if !problems.is_empty() {
        anyhow::bail!("{}", problems.join("; "));
    }
    crate::notify::send_test(&config.notify);
    Ok(())
}

/// Everything the Settings page shows that isn't an agent or a check:
/// notifications, the other machines, and how this dashboard is served.
pub(super) fn settings_json(home: &Home) -> Result<String> {
    let config = Config::load(&home.config_path())?;
    let n = &config.notify;
    let nodes: Vec<_> = crate::nodes::load(home)
        .unwrap_or_default()
        .into_iter()
        .map(|node| serde_json::json!({ "name": node.name, "ssh": node.ssh, "furmi": node.furmi }))
        .collect();
    Ok(serde_json::json!({
        "config_path": contract_tilde(&home.config_path()),
        "notify": {
            "on": n.on.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
            "ntfy_url": n.ntfy.as_ref().and_then(|x| x.url.clone()).unwrap_or_default(),
            "ntfy_topic": n.ntfy.as_ref().and_then(|x| x.topic.clone()).unwrap_or_default(),
            "telegram_chat_id": n.telegram.as_ref().and_then(|x| x.chat_id.clone()).unwrap_or_default(),
            "problems": crate::notify::problems(n),
        },
        "nodes": nodes,
        "web": {
            "listen": config.web.listen,
            "allowed_hosts": config.web.allowed_hosts,
            "trust_local": config.web.trust_local,
        },
        "defaults": {
            "base": config.defaults.base,
            "setup": config.defaults.setup,
            "commit_guard": config.defaults.commit_guard,
        },
        "runner": {
            "daily_token_budget": config.runner.daily_token_budget.unwrap_or(0),
            "strip_env": config.runner.strip_env,
        },
    })
    .to_string())
}

#[derive(Deserialize)]
struct DefaultsBody {
    mode: String,
    worktree: bool,
    on_finish: String,
    retries: u32,
    retry_delay: String,
    timeout: String,
}

pub(super) fn perform_set_defaults(home: &Home, body: &str) -> Result<()> {
    let f: DefaultsBody = serde_json::from_str(body).context("couldn't read the form")?;
    home.ensure_initialized()?;
    crate::config::set_defaults(
        &home.config_path(),
        &crate::config::DefaultsForm {
            mode: &f.mode,
            worktree: f.worktree,
            on_finish: &f.on_finish,
            retries: f.retries,
            retry_delay: &f.retry_delay,
            timeout: &f.timeout,
        },
    )
}

pub(super) fn perform_set_limits(home: &Home, body: &str) -> Result<()> {
    let form: LimitsBody = serde_json::from_str(body).context("couldn't read the form")?;
    if form.max_parallel == 0 || form.max_parallel_per_repo == 0 {
        anyhow::bail!("both limits must be at least 1");
    }
    home.ensure_initialized()?;
    crate::config::set_runner_limits(
        &home.config_path(),
        form.max_parallel,
        form.max_parallel_per_repo,
        &form.poll_interval,
    )?;
    match form.daily_token_budget {
        Some(tokens) => crate::config::set_daily_budget(&home.config_path(), tokens),
        None => Ok(()),
    }
}

/// Which coding agents are installed here and how to get the others, and how
/// many tasks may run at once.
pub(super) fn agents_json(home: &Home) -> Result<String> {
    let config = Config::load(&home.config_path())?;
    Ok(serde_json::json!({
        "agents": crate::config::agent_statuses(&config),
        "max_parallel": config.runner.max_parallel,
        "max_parallel_per_repo": config.runner.max_parallel_per_repo,
        "poll_interval": crate::duration::format(config.runner.poll_interval),
        "daily_token_budget": config.runner.daily_token_budget.unwrap_or(0),
        "defaults": {
            "mode": config.defaults.mode,
            "worktree": config.defaults.worktree,
            "on_finish": config.defaults.on_finish,
            "retries": config.defaults.retries,
            "retry_delay": crate::duration::format(config.defaults.retry_delay),
            "timeout": crate::duration::format(config.defaults.timeout),
        },
    })
    .to_string())
}

/// The models an agent can run, for the task form's suggestions: the ones its
/// own CLI names (`pi --list-models`), the model it uses when a task pins
/// none, and whether it can be handed one at all.
pub(super) fn models_json(home: &Home, agent: Option<&str>, refresh: bool) -> Result<String> {
    let config = Config::load(&home.config_path())?;
    let name = agent.map(str::trim).filter(|a| !a.is_empty());
    let Some((name, agent)) = name.and_then(|n| config.agents.get_key_value(n)) else {
        // No agent named (a form that doesn't offer a choice): nothing to ask.
        return Ok(serde_json::json!({
            "agent": name,
            "can_pin": false,
            "default": null,
            "models": [],
            "source": "none",
        })
        .to_string());
    };
    let listed = crate::models::list(home, name, agent, refresh);
    Ok(serde_json::json!({
        "agent": name,
        // An agent with no `model_args` can't be handed a model at all: the
        // form hides the field rather than let it fail on submit.
        "can_pin": !agent.model_args.is_empty(),
        "default": agent.model,
        "models": listed.models,
        "source": listed.source,
        "error": listed.error,
    })
    .to_string())
}

/// The setup check: what `furmi doctor` looks at, for the dashboard.
pub(super) fn doctor_json(home: &Home) -> String {
    let checks: Vec<_> = crate::doctor::checks(home)
        .iter()
        .map(|c| serde_json::json!({ "level": c.level(), "message": c.message() }))
        .collect();
    serde_json::json!({ "checks": checks }).to_string()
}

pub(super) fn json_err(e: &anyhow::Error) -> String {
    serde_json::to_string(&serde_json::json!({ "error": format!("{e:#}") }))
        .unwrap_or_else(|_| "{\"error\":\"internal error\"}".to_string())
}

/// A log reply, saying when only its end is there.
pub(super) fn json_log_with(log: &str, truncated: bool) -> String {
    serde_json::to_string(&serde_json::json!({ "log": log, "truncated": truncated }))
        .unwrap_or_else(|_| "{\"log\":\"\",\"truncated\":false}".to_string())
}

/// The last `limit` bytes of a log, starting at a line boundary, and whether
/// anything was cut off.
///
/// A live log is re-read while the modal is open, and a runaway agent's log runs
/// to 64 MB — so the stream asks for the tail of it rather than for the whole
/// thing every second.
pub(super) fn tail_of(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut start = text.len() - limit;
    // Never split a multi-byte character (agent output has plenty), then drop
    // the half line the cut leaves behind.
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let cut = &text[start..];
    let cut = cut.split_once('\n').map(|(_, rest)| rest).unwrap_or(cut);
    (cut.to_string(), true)
}

/// How much of the end of a log the task view carries.
const TAIL_LINES: usize = 30;
const TAIL_BYTES: usize = 6 * 1024;

/// The last lines of a failed or unreviewed task's latest attempt.
fn log_tail(home: &Home, task: &Task) -> Option<String> {
    use crate::task::Status;
    if !matches!(task.status, Status::Failed | Status::NeedsReview) || task.attempts == 0 {
        return None;
    }
    let path = home
        .logs()
        .join(format!("{}.{}.log", task.id, task.attempts));
    let text = std::fs::read_to_string(path).ok()?;
    let (text, _) = tail_of(text.trim_end(), TAIL_BYTES);
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    Some(tail).filter(|t| !t.trim().is_empty())
}

/// Everything the page's task view shows, for a task in any state.
///
/// `/api/tasks/<id>` is the *edit* view: it refuses anything that has already
/// started, because editing it would be a lie. This one only reads.
pub(super) fn task_view_json(home: &Home, id: &str) -> Result<String> {
    crate::spec::validate_id(id).map_err(anyhow::Error::msg)?;
    let config = Config::load(&home.config_path()).unwrap_or_default();
    let task = [home.tasks(), home.archive()]
        .iter()
        .map(|dir| dir.join(format!("{id}.md")))
        .find(|path| path.exists())
        .with_context(|| format!("no task `{id}`"))
        .and_then(|path| Task::load(&path))?;
    let eff = task.spec.resolve(&config.defaults);
    let worktree = home.worktrees().join(&task.id);
    let (mut model, mut model_source) = task.model_in_use(&config);
    // A task that ran before `model_used` was kept still has its answer in the
    // log. Only here, on demand: the tables are pushed over `/api/events` every
    // second, and it won't read a log per row for it.
    if model_source == "none" && task.spec.run.is_none() {
        let log = home
            .logs()
            .join(format!("{}.{}.log", task.id, task.attempts.max(1)));
        if let Some(reported) = crate::run::model_from_log(&log) {
            model = reported;
            model_source = "log";
        }
    }
    let view = serde_json::json!({
        "id": task.id,
        // The task's own one-line title, when it has one: what the drawer's
        // heading shows above the id.
        "title": task.spec.display_title(),
        // The whole prompt: this is the one place the page shows it.
        "prompt": task.spec.ask(&task.prompt),
        "command": task.spec.run.is_some(),
        "status": task.status.as_str(),
        "repo": contract_tilde(&eff.repo),
        "branch": task.effective_branch(&config),
        "worktree": eff.worktree.then(|| contract_tilde(&worktree)).filter(|_| worktree.exists()),
        "worktree_off": !eff.worktree,
        "agent": eff.agent,
        "model": model,
        "model_source": model_source,
        "on_finish": on_finish_str(eff.on_finish),
        "mode": match eff.mode {
            crate::spec::Mode::Headless => "headless",
            crate::spec::Mode::Interactive => "interactive",
        },
        "created_at": task.created_at.map(fmt_time),
        "scheduled_at": task.scheduled_at.map(fmt_time),
        "started_at": task.started_at.map(fmt_time),
        "finished_at": task.finished_at.map(fmt_time),
        "attempts": task.attempts,
        "interruptions": task.interruptions,
        "exit_code": task.exit_code,
        "error": task.error,
        "tokens_in": task.tokens_in,
        "tokens_out": task.tokens_out,
        "after": task.spec.after,
        "priority": eff.priority,
        "base": eff.base,
        "setup": eff.setup,
        "waits": (task.status == crate::task::Status::Pending)
            .then(|| crate::run::blocked_by(home, &task))
            .flatten(),
        "pr_url": task.pr_url,
        "created_by": task.created_by,
        // The end of what it printed, for a run that didn't end well: the
        // reason is usually in the last lines, and reading it shouldn't take
        // another click.
        "log_tail": log_tail(home, &task),
        // What it changed, for anything that has run in a git workspace.
        "changes": (task.attempts > 0 && eff.repo.join(".git").exists())
            .then(|| {
                crate::git::changes(
                    &eff.repo,
                    eff.worktree.then_some(worktree.as_path()),
                    task.effective_branch(&config).as_deref(),
                )
            })
            .flatten(),
    });
    Ok(view.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_body_parses_reason_or_defaults() {
        let with: ActionBody = serde_json::from_str(r#"{"reason":"bad approach"}"#).unwrap();
        assert_eq!(with.reason.as_deref(), Some("bad approach"));

        let empty: ActionBody = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.reason, None);
    }

    #[test]
    fn json_err_and_json_log_produce_valid_json() {
        let err = json_err(&anyhow::anyhow!("boom"));
        let v: serde_json::Value = serde_json::from_str(&err).unwrap();
        assert_eq!(v["error"], "boom");

        let log = json_log_with("line one\nline two", false);
        let v: serde_json::Value = serde_json::from_str(&log).unwrap();
        assert_eq!(v["log"], "line one\nline two");
        assert_eq!(v["truncated"], false);

        let long = "a".repeat(100) + "," + &"b".repeat(100);
        let (tail, cut) = tail_of(&long, 100);
        assert!(cut);
        assert!(long.ends_with(&tail));
        assert!(!tail.contains('\n'), "a cut log starts at a line");
        assert_eq!(tail.len(), 100);
        // Nothing to cut: the whole thing comes back untouched.
        assert_eq!(tail_of("short", 100), ("short".to_string(), false));
        // A cut inside a multi-byte character moves forward to a whole one.
        let wide = format!("{}\n{}", "é".repeat(60), "ü".repeat(60));
        let (tail, cut) = tail_of(&wide, 61);
        assert!(cut);
        assert!(!tail.contains('\u{fffd}'));
        assert!("éü".contains(tail.chars().next().unwrap()));
    }

    #[test]
    fn a_task_placed_here_by_remote_runs_in_this_machines_clone() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        for d in home.dirs() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.config_path(), "[defaults]\nagent = \"claude\"\n").unwrap();
        let repo = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q"][..],
            &["remote", "add", "origin", "git@github.com:me/app.git"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(repo.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let cwd = repo.path().canonicalize().unwrap();
        let cwd = cwd.to_str().unwrap();

        let form = |remote: &str| format!(r#"{{"prompt":"do it","remote":"{remote}"}}"#);
        let id = perform_create(&home, Some(cwd), &form("github.com/me/app")).unwrap();

        let task = crate::task::Task::load(&home.tasks().join(format!("{id}.md"))).unwrap();
        assert_eq!(task.remote.as_deref(), Some("github.com/me/app"));
        assert_eq!(
            crate::home::expand_tilde(&task.spec.repo),
            std::path::PathBuf::from(cwd)
        );
        let err = perform_create(&home, Some(cwd), &form("github.com/me/other")).unwrap_err();
        assert!(format!("{err:#}").contains("no clone of github.com/me/other"));
        assert!(perform_create(&home, Some(cwd), r#"{"prompt":"x"}"#).is_err());
    }

    #[test]
    fn a_schedule_placed_here_by_remote_uses_this_machines_clone() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        for d in home.dirs() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.config_path(), "[defaults]\nagent = \"claude\"\n").unwrap();
        let repo = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q"][..],
            &["remote", "add", "origin", "git@github.com:me/app.git"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(repo.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let cwd = repo.path().canonicalize().unwrap();
        let cwd = cwd.to_str().unwrap();
        let form = |remote: &str| {
            format!(r#"{{"prompt":"nightly","remote":"{remote}","cron":"0 3 * * *"}}"#)
        };

        let id = perform_create_schedule(&home, Some(cwd), &form("github.com/me/app")).unwrap();

        let s =
            crate::schedule::Schedule::load(&home.schedules().join(format!("{id}.md"))).unwrap();
        assert_eq!(
            crate::home::expand_tilde(&s.spec.repo),
            std::path::PathBuf::from(cwd)
        );
        assert!(perform_create_schedule(&home, Some(cwd), &form("github.com/me/other")).is_err());
        let edit = perform_edit_schedule(&home, &id, &form("github.com/me/app"));
        assert!(format!("{:#}", edit.unwrap_err()).contains("by its folder"));
    }

    #[test]
    fn the_form_pins_a_model_and_gives_it_back_again() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        for d in home.dirs() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.config_path(), "[defaults]\nagent = \"claude\"\n").unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let path = repo.path().display().to_string();
        let saved = |home: &Home, id: &str| {
            crate::task::Task::load(&home.tasks().join(format!("{id}.md")))
                .unwrap()
                .spec
                .model
        };

        let form = format!(r#"{{"prompt":"do it","repo":"{path}","model":"opus"}}"#);
        let id = perform_create(&home, None, &form).unwrap();
        assert_eq!(saved(&home, &id).as_deref(), Some("opus"));
        // The edit form starts from what the file pins, not from what will run.
        assert_eq!(
            crate::edit::task_detail(&home, &id).unwrap()["model"],
            "opus"
        );

        let edit = format!(r#"{{"prompt":"do it","repo":"{path}","model":"sonnet"}}"#);
        perform_edit_task(&home, &id, &edit).unwrap();
        assert_eq!(saved(&home, &id).as_deref(), Some("sonnet"));

        // An empty box means the agent's own model: the pin goes away.
        let cleared = format!(r#"{{"prompt":"do it","repo":"{path}","model":""}}"#);
        perform_edit_task(&home, &id, &cleared).unwrap();
        assert_eq!(saved(&home, &id), None);

        // A form that doesn't carry the field at all leaves the pin alone.
        perform_edit_task(&home, &id, &form).unwrap();
        let without = format!(r#"{{"prompt":"do it","repo":"{path}"}}"#);
        perform_edit_task(&home, &id, &without).unwrap();
        assert_eq!(saved(&home, &id).as_deref(), Some("opus"));
    }

    #[test]
    fn a_schedule_keeps_the_model_its_form_pins() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        for d in home.dirs() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.config_path(), "[defaults]\nagent = \"claude\"\n").unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let path = repo.path().display().to_string();

        let form =
            format!(r#"{{"prompt":"nightly","repo":"{path}","cron":"0 3 * * *","model":"opus"}}"#);
        let id = perform_create_schedule(&home, None, &form).unwrap();
        let schedule = |home: &Home| {
            crate::schedule::Schedule::load(&home.schedules().join(format!("{id}.md")))
                .unwrap()
                .spec
                .model
        };
        assert_eq!(schedule(&home).as_deref(), Some("opus"));

        let cleared =
            format!(r#"{{"prompt":"nightly","repo":"{path}","cron":"0 3 * * *","model":""}}"#);
        perform_edit_schedule(&home, &id, &cleared).unwrap();
        assert_eq!(schedule(&home), None);
    }

    #[test]
    fn models_json_offers_what_the_agent_names_and_says_if_it_takes_one() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        for d in home.dirs() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            home.config_path(),
            r#"[defaults]
agent = "asks"

[agents.asks]
headless = ["x"]
interactive = ["x"]
model_args = ["--model", "{model}"]
model = "its/default"
models_cmd = ["printf", "p/one\np/two\n"]

[agents.mute]
headless = ["x"]
interactive = ["x"]
"#,
        )
        .unwrap();

        let answer = |body: Result<String>| {
            serde_json::from_str::<serde_json::Value>(&body.unwrap()).unwrap()
        };
        let asks = answer(models_json(&home, Some("asks"), false));
        assert_eq!(asks["can_pin"], true);
        assert_eq!(asks["default"], "its/default");
        assert_eq!(asks["models"], serde_json::json!(["p/one", "p/two"]));
        assert_eq!(asks["source"], "agent");

        // No `model_args`: the agent can't be handed a model, the form hides it.
        let mute = answer(models_json(&home, Some("mute"), false));
        assert_eq!(mute["can_pin"], false);
        assert!(mute["models"].as_array().unwrap().is_empty());

        // No agent named (a form with nothing to choose): nothing to ask.
        let none = answer(models_json(&home, None, false));
        assert_eq!(none["can_pin"], false);
        assert_eq!(none["source"], "none");
    }

    #[test]
    fn a_scratch_task_gets_a_folder_of_its_own_kept_until_clean() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        for d in home.dirs() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.config_path(), "[defaults]\nagent = \"claude\"\n").unwrap();

        let id =
            perform_create(&home, None, r#"{"prompt":"look into it","scratch":true}"#).unwrap();

        let task = crate::task::Task::load(&home.tasks().join(format!("{id}.md"))).unwrap();
        let eff = task
            .spec
            .resolve(&Config::load(&home.config_path()).unwrap().defaults);
        assert_eq!(eff.repo, home.scratch().join(&id));
        assert!(eff.repo.is_dir());
        assert!(!eff.worktree);
        assert_eq!(eff.on_finish, OnFinish::None);
        assert!(task.remote.is_none());
    }
}
