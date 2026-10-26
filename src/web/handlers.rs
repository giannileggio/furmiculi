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
        _ => unreachable!("route only matches ACTIONS"),
    }
}

/// The "New task" form.
#[derive(Deserialize)]
struct NewTaskBody {
    prompt: String,
    repo: String,
    /// `asap` (or empty), or a delay like `1h`.
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
}

/// Queues the task described by the form, as `furmi add` would. Returns its id.
pub(super) fn perform_create(home: &Home, body: &str) -> Result<String> {
    let form: NewTaskBody = serde_json::from_str(body).context("couldn't read the form")?;
    let repo = form.repo.trim();
    if repo.is_empty() {
        anyhow::bail!("choose the repository the agent should work in");
    }
    // A new task has nothing to "keep": no start time means as soon as possible.
    let scheduled_at = match edit::parse_start(form.start.as_deref())? {
        Start::At(at) => Some(at),
        Start::Keep | Start::Asap => None,
    };
    let config = Config::load(&home.config_path())?;
    let spec = TaskSpec {
        repo: repo.into(),
        branch: None,
        worktree: None,
        agent: form.agent.filter(|a| !a.trim().is_empty()),
        model: None,
        mode: None,
        on_finish: form.on_finish,
        retries: None,
        retry_delay: None,
        timeout: None,
    };
    let created = create_task(
        home,
        &config,
        NewTask {
            prompt: form.prompt,
            id: None,
            spec,
            scheduled_at,
            created_by: "web".into(),
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
    prompt: String,
    repo: String,
    /// `keep` (or empty), `asap`, or a delay like `1h`.
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
}

pub(super) fn perform_edit_task(home: &Home, id: &str, body: &str) -> Result<()> {
    let form: EditTaskBody = serde_json::from_str(body).context("couldn't read the form")?;
    edit::edit_task(
        home,
        id,
        TaskEdit {
            prompt: form.prompt,
            repo: form.repo,
            start: edit::parse_start(form.start.as_deref())?,
            on_finish: form.on_finish,
            agent: form.agent,
        },
    )
}

/// The schedule form, for creating and editing alike.
#[derive(Deserialize)]
struct ScheduleBody {
    prompt: String,
    repo: String,
    cron: String,
    #[serde(default)]
    on_finish: Option<OnFinish>,
    #[serde(default)]
    agent: Option<String>,
}

fn schedule_form(body: &str) -> Result<ScheduleEdit> {
    let form: ScheduleBody = serde_json::from_str(body).context("couldn't read the form")?;
    Ok(ScheduleEdit {
        prompt: form.prompt,
        repo: form.repo,
        cron: form.cron,
        on_finish: form.on_finish,
        agent: form.agent,
    })
}

pub(super) fn perform_create_schedule(home: &Home, body: &str) -> Result<String> {
    edit::create_schedule(home, schedule_form(body)?)
}

pub(super) fn perform_edit_schedule(home: &Home, id: &str, body: &str) -> Result<()> {
    edit::edit_schedule(home, id, schedule_form(body)?)
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

/// Which coding agents are installed here and how to get the others.
pub(super) fn agents_json(home: &Home) -> Result<String> {
    let config = Config::load(&home.config_path())?;
    Ok(serde_json::json!({ "agents": crate::config::agent_statuses(&config) }).to_string())
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
/// to 64 MB — so the page asks for the tail of it rather than for the whole
/// thing every couple of seconds.
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
    // log. Only here, on demand: the tables are polled every couple of seconds
    // and won't read a log per row for it.
    if model_source == "none" {
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
        // The whole prompt: this is the one place the page shows it.
        "prompt": task.prompt,
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
}
