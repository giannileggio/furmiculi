//! Starting pending tasks: the parallelism limits, worktrees, and the tmux
//! window that runs `furmi exec`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};

use crate::config::Config;
use crate::git;
use crate::home::{Home, contract_tilde};
use crate::spec::{Effective, Mode};
use crate::task::{Status, Task};
use crate::tmux;

use super::budget::over_budget;
use super::finishing::{archive, fail_attempt};
use super::log::log;
use super::{load_tasks, log_file, status_file};

pub(super) fn start_pending(
    home: &Home,
    config: &Config,
    now: DateTime<FixedOffset>,
) -> Result<()> {
    if over_budget(home, config, now) {
        return Ok(());
    }
    let mut per_repo: HashMap<PathBuf, usize> = HashMap::new();
    // Repos where a `worktree: false` task is running: a second one there
    // would have its branch switched and its work committed under the first.
    let mut in_place: HashSet<PathBuf> = HashSet::new();
    let running = load_tasks(home, Status::Running)?;
    for (_, t) in &running {
        let eff = t.spec.resolve(&config.defaults);
        let key = git::repo_key(&eff.repo);
        *per_repo.entry(key.clone()).or_default() += 1;
        if !eff.worktree {
            in_place.insert(key);
        }
    }
    let mut total = running.len();

    let mut pending: Vec<_> = load_tasks(home, Status::Pending)?
        .into_iter()
        .filter(|(_, t)| t.scheduled_at.is_none_or(|at| at <= now))
        .collect();
    pending.sort_by_key(|(_, t)| (t.scheduled_at.or(t.created_at), t.id.clone()));

    for (path, task) in pending {
        if total >= config.runner.max_parallel {
            break;
        }
        let eff = task.spec.resolve(&config.defaults);
        let key = git::repo_key(&eff.repo);
        let in_repo = per_repo.entry(key.clone()).or_default();
        if *in_repo >= config.runner.max_parallel_per_repo {
            continue;
        }
        if !eff.worktree && in_place.contains(&key) {
            log(&format!(
                "{} waits: another task is running without a worktree in {}",
                task.id,
                key.display()
            ));
            continue;
        }
        match start(home, config, &path, task, now) {
            Ok(true) => {
                total += 1;
                *in_repo += 1;
                if !eff.worktree {
                    in_place.insert(key);
                }
            }
            Ok(false) => {}
            Err(e) => log(&format!("starting {}: {e:#}", path.display())),
        }
    }
    Ok(())
}

/// Starts one attempt. Returns whether the task is now running.
fn start(
    home: &Home,
    config: &Config,
    path: &Path,
    mut task: Task,
    now: DateTime<FixedOffset>,
) -> Result<bool> {
    let eff = task.spec.resolve(&config.defaults);
    let problems = start_problems(config, &task, path);
    if !problems.is_empty() {
        task.status = Status::Failed;
        task.error = Some(format!("invalid task: {}", problems.join("; ")));
        archive(home, path, task, now)?;
        return Ok(false);
    }

    task.attempts += 1;
    task.started_at = Some(now);
    let session = &config.runner.tmux_session;
    let launched = prepare_workdir(home, config, &task, &eff).and_then(|dir| {
        // A leftover window with the same name would make targets ambiguous.
        tmux::kill_window(session, &task.id)?;
        // A record left by an earlier task with this id would end this
        // attempt as soon as it's polled.
        let status = status_file(home, &task.id, task.attempts);
        remove_if_exists(&status)?;
        remove_if_exists(&log_file(home, &task.id, task.attempts))?;
        tmux::spawn(
            session,
            &task.id,
            &dir,
            &exec_argv(home, &task.id, &status)?,
        )
    });
    match launched {
        Ok(()) => {
            task.status = Status::Running;
            task.tmux_window = Some(format!("{session}:{}", task.id));
            task.save(path)?;
            if !eff.worktree {
                log(&format!(
                    "warning: {} runs directly in {} (worktree: false): the agent edits that \
                     checkout and `on_finish` commits everything in it",
                    task.id,
                    contract_tilde(&eff.repo)
                ));
            }
            log(&format!(
                "started {} (attempt {}) in tmux {session}:{}",
                task.id, task.attempts, task.id
            ));
            Ok(true)
        }
        Err(e) => {
            fail_attempt(
                home,
                path,
                task,
                &eff,
                format!("could not start: {e:#}"),
                true,
                now,
            )?;
            Ok(false)
        }
    }
}

/// Everything that stops a task from starting: `Task::problems` plus
/// runner-only checks. Chiefly, whether the agent's program can actually be
/// run — that is deliberately not part of `Task::problems`, so a task may
/// still be queued (and validated) before its agent is installed.
fn start_problems(config: &Config, task: &Task, path: &Path) -> Vec<String> {
    let mut out = task.problems(config, Some(path));
    let eff = task.spec.resolve(&config.defaults);
    if let Some(name) = eff.agent.as_deref()
        && let Some(agent) = config.agents.get(name)
    {
        let template = match eff.mode {
            Mode::Headless => &agent.headless,
            Mode::Interactive => &agent.interactive,
        };
        if let Some(program) = template.first()
            && !crate::config::on_path(program)
            && !Path::new(program).is_file()
        {
            out.push(format!(
                "agent `{name}`: `{program}` is not on $PATH; install it or fix \
                 [agents.{name}] in config.toml (`furmi doctor` shows the setup)"
            ));
        }
    }
    out
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

pub(super) fn workdir(home: &Home, task: &Task, eff: &Effective) -> PathBuf {
    if eff.worktree {
        home.worktrees().join(&task.id)
    } else {
        eff.repo.clone()
    }
}

fn prepare_workdir(home: &Home, config: &Config, task: &Task, eff: &Effective) -> Result<PathBuf> {
    let dir = workdir(home, task, eff);
    match task.effective_branch(config) {
        Some(branch) if eff.worktree => git::ensure_worktree(&eff.repo, &dir, &branch)?,
        Some(branch) => git::checkout_branch(&eff.repo, &branch)?,
        None => {}
    }
    Ok(dir)
}

fn exec_argv(home: &Home, id: &str, status_file: &Path) -> Result<Vec<String>> {
    let exe = crate::exe::path().context("locating the furmi binary")?;
    let path = |p: &Path| {
        p.to_str()
            .map(String::from)
            .context("path is not valid UTF-8")
    };
    Ok(vec![
        path(&exe)?,
        "--home".into(),
        path(home.root())?,
        "exec".into(),
        id.into(),
        "--status-file".into(),
        path(status_file)?,
    ])
}
