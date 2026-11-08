//! Changing tasks and schedules after they were created, for the dashboard:
//! edit a task that hasn't started, and create, edit, pause, resume and
//! delete schedules.
//!
//! Like `furmi done|cancel|retry`, these hold the home lock while they touch
//! files, so they never race a `tick` of `furmi run`, which also rewrites
//! schedule files (`last_enqueued_at`). Every edit starts from the file as it
//! is now and changes only what the form shows, so fields the form doesn't
//! know about (`branch`, `model`, `timeout`, `allow_overlap`...) survive.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset};

use crate::config::Config;
use crate::home::{Home, contract_tilde, expand_tilde};
use crate::schedule::Schedule;
use crate::spec::{Mode, OnFinish, TaskSpec, slugify, validate_id};
use crate::task::{Status, Task, now};

/// When a task should start, as the forms say it.
#[derive(Debug, PartialEq)]
pub enum Start {
    /// Leave `scheduled_at` as it is.
    Keep,
    Asap,
    At(DateTime<FixedOffset>),
}

/// `keep` (or nothing), `asap`, or a delay like `1h`.
pub fn parse_start(text: Option<&str>) -> Result<Start> {
    match text.map(str::trim) {
        None | Some("") | Some("keep") => Ok(Start::Keep),
        Some("asap") => Ok(Start::Asap),
        Some(delay) => {
            let delay: Duration = humantime::parse_duration(delay)
                .map_err(|e| anyhow::anyhow!("can't start after `{delay}`: {e}"))?;
            let delay = chrono::Duration::from_std(delay).context("that delay is too long")?;
            Ok(Start::At(now() + delay))
        }
    }
}

/// One of a spec's own options as a form carries it: not sent (leave it as it
/// is), sent empty (unpin it, so `[defaults]` applies), or sent with a value
/// (pin that one).
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Choice<T> {
    #[default]
    Leave,
    Clear,
    Pin(T),
}

impl<T> Choice<T> {
    fn apply(self, field: &mut Option<T>) {
        match self {
            Choice::Leave => {}
            Choice::Clear => *field = None,
            Choice::Pin(value) => *field = Some(value),
        }
    }
}

/// The options a form can pin beyond the ones it has a field of its own for.
#[derive(Debug, Default, PartialEq)]
pub struct SpecChoices {
    pub branch: Choice<String>,
    pub mode: Choice<Mode>,
    pub worktree: Choice<bool>,
    pub retries: Choice<u32>,
    pub retry_delay: Choice<Duration>,
    pub timeout: Choice<Duration>,
}

/// What the task form edits. `on_finish` and `agent` left as `None` stay as
/// they are.
pub struct TaskEdit {
    pub prompt: String,
    pub repo: String,
    pub start: Start,
    pub on_finish: Option<OnFinish>,
    pub agent: Option<String>,
    /// `None` leaves the model as it is, `Some("")` clears it (the agent's own
    /// model applies), anything else pins that one.
    pub model: Option<String>,
    pub choices: SpecChoices,
}

/// What the schedule form edits.
pub struct ScheduleEdit {
    pub prompt: String,
    pub repo: String,
    pub cron: String,
    pub on_finish: Option<OnFinish>,
    pub agent: Option<String>,
    /// As `TaskEdit::model`.
    pub model: Option<String>,
    pub choices: SpecChoices,
}

fn load_config(home: &Home) -> Result<Config> {
    home.ensure_initialized()?;
    Config::load(&home.config_path())
}

pub(crate) fn path_of(repo: &str) -> Result<PathBuf> {
    let repo = repo.trim();
    if repo.is_empty() {
        bail!("choose the repository the agent should work in");
    }
    Ok(PathBuf::from(contract_tilde(&std::path::absolute(
        expand_tilde(std::path::Path::new(repo)),
    )?)))
}

fn problems_error(problems: Vec<String>) -> anyhow::Error {
    anyhow::anyhow!("{}", problems.join("; "))
}

// --- tasks -------------------------------------------------------------------

/// The fields the edit form starts from, for a task that can still be edited.
pub fn task_detail(home: &Home, id: &str) -> Result<serde_json::Value> {
    let config = load_config(home)?;
    let (_, task) = load_pending(home, id)?;
    let eff = task.spec.resolve(&config.defaults);
    Ok(with_pinned(
        serde_json::json!({
            "id": task.id,
            "prompt": task.spec.ask(&task.prompt),
            "command": task.spec.run.is_some(),
            "repo": contract_tilde(&eff.repo),
            "on_finish": on_finish_str(eff.on_finish),
            "agent": eff.agent,
            // The model the file pins, not the one that will run: an empty field in
            // the form must mean "keep using the agent's own", not re-pin it.
            "model": task.spec.model,
            "starts": task.scheduled_at.map(crate::cmd::fmt_time),
            "has_run": task.attempts > 0,
            "draft": task.status == Status::Draft,
        }),
        &task.spec,
    ))
}

/// What the spec itself pins, for a form: only the pinned values, so a field
/// left empty in the form means "not pinned, `[defaults]` applies".
pub fn with_pinned(mut detail: serde_json::Value, spec: &TaskSpec) -> serde_json::Value {
    let pinned = serde_json::json!({
        "branch": spec.branch,
        "mode": spec.mode.map(mode_str),
        "worktree": spec.worktree,
        "retries": spec.retries,
        "retry_delay": spec.retry_delay.map(duration_str),
        "timeout": spec.timeout.map(duration_str),
    });
    if let (Some(map), serde_json::Value::Object(pinned)) = (detail.as_object_mut(), pinned) {
        map.extend(pinned);
    }
    detail
}

fn duration_str(d: Duration) -> String {
    humantime::format_duration(d).to_string()
}

fn load_pending(home: &Home, id: &str) -> Result<(PathBuf, Task)> {
    validate_id(id).map_err(anyhow::Error::msg)?;
    let path = home.tasks().join(format!("{id}.md"));
    if !path.exists() {
        bail!("no waiting task `{id}`: it may have finished, or been cancelled");
    }
    let task = Task::load(&path)?;
    if !matches!(task.status, Status::Pending | Status::Draft) {
        bail!(
            "`{id}` is {}; only tasks that haven't started can be edited. \
             Cancel it and add a new one instead",
            task.status.as_str()
        );
    }
    Ok((path, task))
}

/// Changes a waiting task. A task that has already run (and was sent back to
/// the queue) keeps its project: its workspace belongs to it.
pub fn edit_task(home: &Home, id: &str, edit: TaskEdit) -> Result<()> {
    let config = load_config(home)?;
    let _lock = home.lock()?;
    // Inside the lock: the runner can't have started it since we looked.
    let (path, mut task) = load_pending(home, id)?;

    let repo = path_of(&edit.repo)?;
    // A scratch task's folder is its project; moving it, or committing from it,
    // would turn it into something else.
    if expand_tilde(&task.spec.repo).starts_with(home.scratch()) {
        if expand_tilde(&repo) != expand_tilde(&task.spec.repo) {
            bail!("a task with no project can't be moved to one");
        }
        if edit.on_finish.is_some_and(|f| f != OnFinish::None) {
            bail!("a task with no project has nothing to commit, push or open a PR for");
        }
    }
    if task.attempts > 0 && expand_tilde(&repo) != expand_tilde(&task.spec.repo) {
        bail!("this task has already run in its project, so it can't move to another one");
    }
    task.spec.repo = repo;
    task.fill_remote();
    set_ask(&mut task.spec, &mut task.prompt, &edit.prompt);
    match edit.start {
        Start::Keep => {}
        Start::Asap => task.scheduled_at = None,
        Start::At(at) => task.scheduled_at = Some(at),
    }
    apply_choices(&mut task.spec, edit.on_finish, edit.agent);
    apply_model(&mut task.spec, edit.model);
    apply_spec_choices(&mut task.spec, edit.choices);

    let mut problems = task.problems(&config, Some(&path));
    if task.prompt.is_empty() && task.spec.run.is_none() {
        problems.push("the prompt is empty".into());
    }
    if !problems.is_empty() {
        return Err(problems_error(problems));
    }
    task.save(&path)
}

pub(crate) fn apply_choices(
    spec: &mut TaskSpec,
    on_finish: Option<OnFinish>,
    agent: Option<String>,
) {
    if on_finish.is_some() {
        spec.on_finish = on_finish;
    }
    if let Some(agent) = agent.filter(|a| !a.trim().is_empty() && spec.run.is_none()) {
        spec.agent = Some(agent);
    }
}

/// The options the form carries as text: what it sent a value for is pinned,
/// what it sent empty is unpinned, and what it left out stays as it is.
pub(crate) fn apply_spec_choices(spec: &mut TaskSpec, choices: SpecChoices) {
    choices.branch.apply(&mut spec.branch);
    choices.mode.apply(&mut spec.mode);
    choices.worktree.apply(&mut spec.worktree);
    choices.retries.apply(&mut spec.retries);
    choices.retry_delay.apply(&mut spec.retry_delay);
    choices.timeout.apply(&mut spec.timeout);
}

/// The form's model box: empty means the agent's own model, so it clears the
/// field. A command task has no model to set (its own validation says so).
fn apply_model(spec: &mut TaskSpec, model: Option<String>) {
    if spec.run.is_some() {
        return;
    }
    if let Some(model) = model {
        let model = model.trim();
        spec.model = if model.is_empty() {
            None
        } else {
            Some(model.to_string())
        };
    }
}

/// Stores what the form's one text box says: a command task's command, else
/// the prompt.
pub(crate) fn set_ask(spec: &mut TaskSpec, prompt: &mut String, text: &str) {
    let text = text.trim().to_string();
    match &mut spec.run {
        Some(run) => *run = text,
        None => *prompt = text,
    }
}

pub fn on_finish_str(on_finish: OnFinish) -> &'static str {
    match on_finish {
        OnFinish::None => "none",
        OnFinish::Commit => "commit",
        OnFinish::Push => "push",
        OnFinish::Pr => "pr",
    }
}

pub fn mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::Headless => "headless",
        Mode::Interactive => "interactive",
    }
}

// --- schedules ---------------------------------------------------------------

fn schedule_path(home: &Home, id: &str) -> Result<PathBuf> {
    validate_id(id).map_err(anyhow::Error::msg)?;
    let path = home.schedules().join(format!("{id}.md"));
    if !path.exists() {
        bail!("no schedule `{id}`");
    }
    Ok(path)
}

pub fn schedule_detail(home: &Home, id: &str) -> Result<serde_json::Value> {
    let config = load_config(home)?;
    let s = Schedule::load(&schedule_path(home, id)?)?;
    let eff = s.spec.resolve(&config.defaults);
    Ok(with_pinned(
        serde_json::json!({
            "id": s.id,
            "prompt": s.spec.ask(&s.prompt),
            "command": s.spec.run.is_some(),
            "cron": s.cron,
            "enabled": s.enabled,
            "repo": contract_tilde(&eff.repo),
            "on_finish": on_finish_str(eff.on_finish),
            "agent": eff.agent,
            "model": s.spec.model,
        }),
        &s.spec,
    ))
}

/// Adds a schedule, with an id taken from the first line of its prompt.
pub fn create_schedule(home: &Home, new: ScheduleEdit) -> Result<String> {
    let mut spec = blank_spec(path_of(&new.repo)?);
    apply_choices(&mut spec, new.on_finish, new.agent);
    apply_model(&mut spec, new.model);
    apply_spec_choices(&mut spec, new.choices);
    add_schedule(home, None, new.cron, false, spec, new.prompt)
}

pub(crate) fn blank_spec(repo: PathBuf) -> TaskSpec {
    TaskSpec {
        repo,
        branch: None,
        worktree: None,
        agent: None,
        model: None,
        mode: None,
        on_finish: None,
        retries: None,
        retry_delay: None,
        timeout: None,
        run: None,
    }
}

/// Validates and writes a new schedule: the one place schedule files are
/// created, for the dashboard and `furmi schedule add`. `id` is taken from the
/// first line of the prompt if `None`. Returns the id.
pub fn add_schedule(
    home: &Home,
    id: Option<String>,
    cron: String,
    allow_overlap: bool,
    spec: TaskSpec,
    prompt: String,
) -> Result<String> {
    let config = load_config(home)?;
    let _lock = home.lock()?;
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() && spec.run.is_none() {
        bail!("the prompt is empty");
    }
    let taken = |id: &str| home.schedules().join(format!("{id}.md")).exists();
    let id = match id {
        Some(id) => {
            validate_id(&id).map_err(anyhow::Error::msg)?;
            if taken(&id) {
                bail!("a schedule with id `{id}` already exists");
            }
            id
        }
        None => {
            let base = slugify(spec.ask(&prompt).lines().next().unwrap_or_default(), 48);
            (1..)
                .map(|n| {
                    if n == 1 {
                        base.clone()
                    } else {
                        format!("{base}-{n}")
                    }
                })
                .find(|id| !taken(id))
                .expect("infinite iterator")
        }
    };
    let mut spec = spec;
    spec.repo = path_of(&spec.repo.to_string_lossy())?;
    let schedule = Schedule {
        id: id.clone(),
        cron: cron.trim().to_string(),
        enabled: true,
        allow_overlap,
        spec,
        last_enqueued_at: None,
        prompt,
    };
    let path = home.schedules().join(format!("{id}.md"));
    check_schedule(&schedule, &config, &path)?;
    schedule.save(&path)?;
    Ok(id)
}

fn check_schedule(schedule: &Schedule, config: &Config, path: &std::path::Path) -> Result<()> {
    let problems = schedule.problems(config, Some(path));
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems_error(problems))
    }
}

pub fn edit_schedule(home: &Home, id: &str, edit: ScheduleEdit) -> Result<()> {
    update_schedule(home, id, |s| {
        s.cron = edit.cron.trim().to_string();
        set_ask(&mut s.spec, &mut s.prompt, &edit.prompt);
        s.spec.repo = path_of(&edit.repo)?;
        apply_choices(&mut s.spec, edit.on_finish, edit.agent);
        apply_model(&mut s.spec, edit.model);
        apply_spec_choices(&mut s.spec, edit.choices);
        Ok(())
    })
}

/// Changes a schedule in place: `change` sees the file as it is now, under the
/// lock, and whatever it leaves alone survives. Validated before saving.
pub fn update_schedule(
    home: &Home,
    id: &str,
    change: impl FnOnce(&mut Schedule) -> Result<()>,
) -> Result<()> {
    let config = load_config(home)?;
    let _lock = home.lock()?;
    let path = schedule_path(home, id)?;
    let mut s = Schedule::load(&path)?;
    let old_cron = s.cron.clone();
    change(&mut s)?;
    s.cron = s.cron.trim().to_string();
    s.prompt = s.prompt.trim().to_string();
    if s.cron != old_cron {
        // Otherwise a slot the new cron says has already passed would fire at
        // once, for a schedule the user only just changed.
        s.last_enqueued_at = Some(now());
    }
    check_schedule(&s, &config, &path)?;
    s.save(&path)
}

/// Pauses or resumes a schedule. Resuming starts counting from now, so it
/// doesn't fire for a slot that passed while it was paused.
pub fn set_schedule_enabled(home: &Home, id: &str, enabled: bool) -> Result<()> {
    home.ensure_initialized()?;
    let _lock = home.lock()?;
    let path = schedule_path(home, id)?;
    let mut s = Schedule::load(&path)?;
    if s.enabled == enabled {
        return Ok(());
    }
    s.enabled = enabled;
    if enabled {
        s.last_enqueued_at = Some(now());
    }
    s.save(&path)
}

/// Deletes the schedule. Tasks it already queued stay.
pub fn delete_schedule(home: &Home, id: &str) -> Result<()> {
    home.ensure_initialized()?;
    let _lock = home.lock()?;
    let path = schedule_path(home, id)?;
    std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))
}

/// Ids of all schedule files, for tests and callers that need to list them.
#[cfg(test)]
fn schedule_ids(home: &Home) -> Vec<String> {
    crate::cmd::md_files(&home.schedules())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{NewTask, create_task};

    /// An initialized home, and a repo it can run tasks in.
    fn setup() -> (tempfile::TempDir, tempfile::TempDir, Home, Config) {
        let dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        for d in home.dirs() {
            std::fs::create_dir_all(d).unwrap();
        }
        let config = Config::parse("[defaults]\nagent = \"claude\"\n").unwrap();
        std::fs::write(home.config_path(), "[defaults]\nagent = \"claude\"\n").unwrap();
        (dir, repo, home, config)
    }

    fn spec(repo: &std::path::Path) -> TaskSpec {
        TaskSpec {
            repo: repo.to_path_buf(),
            branch: Some("keep-me".into()),
            worktree: None,
            agent: None,
            model: Some("a-model".into()),
            mode: None,
            on_finish: None,
            retries: None,
            retry_delay: None,
            timeout: None,
            run: None,
        }
    }

    fn queue(home: &Home, config: &Config, repo: &std::path::Path, prompt: &str) -> Task {
        create_task(
            home,
            config,
            NewTask {
                prompt: prompt.into(),
                id: None,
                spec: spec(repo),
                scheduled_at: None,
                created_by: "test".into(),
                draft: false,
            },
        )
        .unwrap()
        .0
    }

    fn edit(repo: &std::path::Path, prompt: &str) -> TaskEdit {
        TaskEdit {
            prompt: prompt.into(),
            repo: repo.display().to_string(),
            start: Start::Keep,
            on_finish: None,
            agent: None,
            model: None,
            choices: SpecChoices::default(),
        }
    }

    #[test]
    fn a_task_with_no_project_stays_without_one() {
        let (_d, repo, home, config) = setup();
        let mut scratch = spec(repo.path());
        scratch.repo = std::path::PathBuf::new();
        scratch.branch = None;
        let make = || {
            create_task(
                &home,
                &config,
                NewTask {
                    prompt: "look into it".into(),
                    id: None,
                    spec: scratch.clone(),
                    scheduled_at: None,
                    created_by: "test".into(),
                    draft: false,
                },
            )
            .unwrap()
            .0
        };
        let task = make();
        let folder = crate::home::expand_tilde(&task.spec.repo);

        // Not moved into a project, nor given something to commit.
        let err = edit_task(&home, &task.id, edit(repo.path(), "x")).unwrap_err();
        assert!(format!("{err:#}").contains("can't be moved"), "{err:#}");
        let mut e = edit(&folder, "x");
        e.on_finish = Some(OnFinish::Commit);
        assert!(edit_task(&home, &task.id, e).is_err());
        edit_task(&home, &task.id, edit(&folder, "reworded")).unwrap();

        // A second one with the same words gets a folder of its own.
        let second = make();
        assert_ne!(second.id, task.id);
        assert_ne!(second.spec.repo, task.spec.repo);
    }

    #[test]
    fn parses_start_choices() {
        assert_eq!(parse_start(None).unwrap(), Start::Keep);
        assert_eq!(parse_start(Some("")).unwrap(), Start::Keep);
        assert_eq!(parse_start(Some("keep")).unwrap(), Start::Keep);
        assert_eq!(parse_start(Some("asap")).unwrap(), Start::Asap);
        let Start::At(at) = parse_start(Some("2h")).unwrap() else {
            panic!("expected a time");
        };
        let from_now = (at - now()).num_minutes();
        assert!((118..=120).contains(&from_now), "{from_now}");
        let err = parse_start(Some("soonish")).unwrap_err().to_string();
        assert!(err.contains("can't start after"), "{err}");
    }

    fn repo_with_origin(url: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [&["init", "-q"][..], &["remote", "add", "origin", url]] {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success();
            assert!(ok);
        }
        dir
    }

    #[test]
    fn moving_a_task_to_another_repo_moves_its_remote() {
        let (_dir, _repo, home, config) = setup();
        let a = repo_with_origin("git@github.com:me/a.git");
        let b = repo_with_origin("https://github.com/me/b");
        let task = queue(&home, &config, a.path(), "Prompt");
        assert_eq!(task.remote.as_deref(), Some("github.com/me/a"));

        edit_task(&home, &task.id, edit(b.path(), "Prompt")).unwrap();

        let saved = Task::load(&home.tasks().join(format!("{}.md", task.id))).unwrap();
        assert_eq!(saved.remote.as_deref(), Some("github.com/me/b"));
    }

    #[test]
    fn the_form_edits_a_command_tasks_command_and_gives_it_no_agent() {
        let (_dir, repo, home, config) = setup();
        let (task, path) = create_task(
            &home,
            &config,
            NewTask {
                prompt: String::new(),
                id: None,
                spec: TaskSpec {
                    run: Some("make backup".into()),
                    ..blank_spec(repo.path().into())
                },
                scheduled_at: None,
                created_by: "test".into(),
                draft: false,
            },
        )
        .unwrap();
        assert_eq!(task.id, "make-backup");
        let detail = task_detail(&home, &task.id).unwrap();
        assert_eq!(detail["prompt"], "make backup");
        assert_eq!(detail["command"], true);

        let mut change = edit(repo.path(), " make full ");
        change.agent = Some("claude".into());
        edit_task(&home, &task.id, change).unwrap();
        let saved = Task::load(&path).unwrap();
        assert_eq!(saved.spec.run.as_deref(), Some("make full"));
        assert_eq!(saved.prompt, "");
        assert_eq!(saved.spec.agent, None);

        // Blank is no more a command than it is a prompt.
        let err = edit_task(&home, &task.id, edit(repo.path(), "  ")).unwrap_err();
        assert!(err.to_string().contains("`run` is empty"), "{err}");
    }

    #[test]
    fn editing_a_task_changes_the_form_fields_and_keeps_the_rest() {
        let (_dir, repo, home, config) = setup();
        let task = queue(&home, &config, repo.path(), "Original prompt");

        let mut change = edit(repo.path(), "  A better prompt  ");
        change.start = Start::At(now() + chrono::Duration::hours(1));
        change.on_finish = Some(OnFinish::Pr);
        edit_task(&home, &task.id, change).unwrap();

        let saved = Task::load(&home.tasks().join(format!("{}.md", task.id))).unwrap();
        assert_eq!(saved.prompt, "A better prompt");
        assert_eq!(saved.spec.on_finish, Some(OnFinish::Pr));
        assert!(saved.scheduled_at.is_some());
        // Not on the form, so untouched.
        assert_eq!(saved.spec.branch.as_deref(), Some("keep-me"));
        assert_eq!(saved.spec.model.as_deref(), Some("a-model"));
        assert_eq!(saved.created_by.as_deref(), Some("test"));

        // "As soon as possible" clears the start time; "keep" leaves it.
        let mut change = edit(repo.path(), "A better prompt");
        change.start = Start::Asap;
        edit_task(&home, &task.id, change).unwrap();
        let saved = Task::load(&home.tasks().join(format!("{}.md", task.id))).unwrap();
        assert_eq!(saved.scheduled_at, None);
    }

    #[test]
    fn only_waiting_tasks_can_be_edited() {
        let (_dir, repo, home, config) = setup();
        let task = queue(&home, &config, repo.path(), "Do it");
        let path = home.tasks().join(format!("{}.md", task.id));

        let mut running = Task::load(&path).unwrap();
        running.status = Status::Running;
        running.save(&path).unwrap();
        let err = edit_task(&home, &task.id, edit(repo.path(), "x")).unwrap_err();
        assert!(err.to_string().contains("haven't started"), "{err}");
        assert!(task_detail(&home, &task.id).is_err());

        let err = edit_task(&home, "no-such-task", edit(repo.path(), "x")).unwrap_err();
        assert!(err.to_string().contains("no waiting task"), "{err}");
    }

    #[test]
    fn a_bad_edit_leaves_the_file_alone() {
        let (_dir, repo, home, config) = setup();
        let task = queue(&home, &config, repo.path(), "Do it");
        let path = home.tasks().join(format!("{}.md", task.id));
        let before = std::fs::read_to_string(&path).unwrap();

        let err = edit_task(&home, &task.id, edit(repo.path(), "   ")).unwrap_err();
        assert!(err.to_string().contains("prompt is empty"), "{err}");
        let mut change = edit(repo.path(), "ok");
        change.repo = "/no/such/dir".into();
        let err = edit_task(&home, &task.id, change).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
        let mut change = edit(repo.path(), "ok");
        change.agent = Some("nonesuch".into());
        assert!(edit_task(&home, &task.id, change).is_err());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_task_that_has_run_cannot_move_projects() {
        let (_dir, repo, home, config) = setup();
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join(".git")).unwrap();
        let task = queue(&home, &config, repo.path(), "Do it");
        let path = home.tasks().join(format!("{}.md", task.id));
        let mut retried = Task::load(&path).unwrap();
        retried.attempts = 1;
        retried.save(&path).unwrap();

        let err = edit_task(&home, &task.id, edit(other.path(), "Do it")).unwrap_err();
        assert!(err.to_string().contains("can't move"), "{err}");
        // Same project, new prompt: fine.
        edit_task(&home, &task.id, edit(repo.path(), "Do it differently")).unwrap();
    }

    fn schedule_form(repo: &std::path::Path, prompt: &str, cron: &str) -> ScheduleEdit {
        ScheduleEdit {
            prompt: prompt.into(),
            repo: repo.display().to_string(),
            cron: cron.into(),
            on_finish: None,
            agent: None,
            model: None,
            choices: SpecChoices::default(),
        }
    }

    #[test]
    fn schedules_are_created_edited_paused_resumed_and_deleted() {
        let (_dir, repo, home, _config) = setup();

        let id = create_schedule(
            &home,
            schedule_form(repo.path(), "Update deps\n\nAnd test", "0 9 * * *"),
        )
        .unwrap();
        assert_eq!(id, "update-deps");
        let again = create_schedule(
            &home,
            schedule_form(repo.path(), "Update deps", "0 9 * * *"),
        )
        .unwrap();
        assert_eq!(again, "update-deps-2");
        assert_eq!(schedule_ids(&home).len(), 2);

        // The runner's bookkeeping and the fields the form doesn't show survive edits.
        let path = home.schedules().join("update-deps.md");
        let mut s = Schedule::load(&path).unwrap();
        s.last_enqueued_at = Some(now() - chrono::Duration::days(3));
        s.allow_overlap = true;
        s.spec.model = Some("a-model".into());
        s.save(&path).unwrap();

        // Same cron: the marker is untouched.
        edit_schedule(
            &home,
            &id,
            schedule_form(repo.path(), "New words", "0 9 * * *"),
        )
        .unwrap();
        let s = Schedule::load(&path).unwrap();
        assert_eq!(s.prompt, "New words");
        assert!(s.allow_overlap);
        assert_eq!(s.spec.model.as_deref(), Some("a-model"));
        assert!(s.last_enqueued_at.unwrap() < now() - chrono::Duration::days(2));

        // New cron: counting restarts from now, so it can't fire for the past.
        edit_schedule(
            &home,
            &id,
            schedule_form(repo.path(), "New words", "30 8 * * 1-5"),
        )
        .unwrap();
        let s = Schedule::load(&path).unwrap();
        assert_eq!(s.cron, "30 8 * * 1-5");
        assert!(s.last_enqueued_at.unwrap() > now() - chrono::Duration::minutes(1));

        // Pause, then resume (which also restarts the count).
        set_schedule_enabled(&home, &id, false).unwrap();
        let mut s = Schedule::load(&path).unwrap();
        assert!(!s.enabled);
        s.last_enqueued_at = Some(now() - chrono::Duration::days(3));
        s.save(&path).unwrap();
        set_schedule_enabled(&home, &id, true).unwrap();
        let s = Schedule::load(&path).unwrap();
        assert!(s.enabled);
        assert!(s.last_enqueued_at.unwrap() > now() - chrono::Duration::minutes(1));
        assert_eq!(schedule_detail(&home, &id).unwrap()["enabled"], true);

        delete_schedule(&home, &id).unwrap();
        assert!(!path.exists());
        assert!(delete_schedule(&home, &id).is_err());
        assert_eq!(schedule_ids(&home), vec!["update-deps-2"]);
    }

    #[test]
    fn schedule_mistakes_are_explained_and_change_nothing() {
        let (_dir, repo, home, _config) = setup();
        let err =
            create_schedule(&home, schedule_form(repo.path(), "x", "every night")).unwrap_err();
        assert!(err.to_string().contains("invalid cron"), "{err}");
        let err =
            create_schedule(&home, schedule_form(repo.path(), "  ", "0 9 * * *")).unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(schedule_ids(&home).is_empty());

        let id = create_schedule(&home, schedule_form(repo.path(), "Fine", "0 9 * * *")).unwrap();
        let path = home.schedules().join(format!("{id}.md"));
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(edit_schedule(&home, &id, schedule_form(repo.path(), "Fine", "nope")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}
