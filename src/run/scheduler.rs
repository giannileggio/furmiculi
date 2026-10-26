//! Enqueuing a task for each due schedule.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, FixedOffset, Local};

use crate::cmd::{md_files, unique_id};
use crate::config::Config;
use crate::home::Home;
use crate::schedule::Schedule;
use crate::task::{Status, Task};

use super::log::log;

pub(super) fn enqueue_schedules(
    home: &Home,
    config: &Config,
    now: DateTime<FixedOffset>,
) -> Result<()> {
    for path in md_files(&home.schedules())? {
        if let Err(e) = enqueue_schedule(home, config, &path, now) {
            log(&format!("schedule {}: {e:#}", path.display()));
        }
    }
    Ok(())
}

fn enqueue_schedule(
    home: &Home,
    config: &Config,
    path: &Path,
    now: DateTime<FixedOffset>,
) -> Result<()> {
    let mut s = Schedule::load(path)?;
    if !s.enabled {
        return Ok(());
    }
    let Some(last) = s.last_enqueued_at else {
        // First sighting: start counting from now instead of firing for a
        // slot that passed before the schedule existed.
        s.last_enqueued_at = Some(now);
        return s.save(path);
    };
    let slot = s
        .latest_at_or_before(now.with_timezone(&Local))?
        .fixed_offset();
    if slot <= last {
        return Ok(());
    }
    // Record the slot even if enqueueing fails, so a broken schedule logs
    // once per firing instead of once per poll.
    s.last_enqueued_at = Some(slot);
    s.save(path)?;

    let problems = s.problems(config, Some(path));
    if !problems.is_empty() {
        anyhow::bail!("not enqueued: {}", problems.join("; "));
    }
    if !s.allow_overlap && has_queued_run(home, &s.id) {
        log(&format!(
            "schedule {}: skipped {}: the previous run is still queued or running \
             (set `allow_overlap: true` to enqueue anyway)",
            s.id,
            slot.format("%Y-%m-%d %H:%M")
        ));
        return Ok(());
    }
    let streak = failure_streak(home, &s.id);
    if streak >= FAILURE_WARNING_STREAK {
        log(&format!(
            "warning: schedule {}: its last {streak} runs all failed",
            s.id
        ));
    }
    let base = format!("{}-{}", truncate_id(&s.id, 48), slot.format("%Y%m%d-%H%M"));
    let mut task = Task::new(unique_id(home, &base), s.spec.clone(), s.prompt.clone());
    task.created_by = Some(format!("schedule:{}", s.id));
    task.create(&home.tasks().join(format!("{}.md", task.id)))?;
    log(&format!("enqueued {} from schedule {}", task.id, s.id));
    Ok(())
}

/// Consecutive failed runs of a schedule at which the runner starts warning.
const FAILURE_WARNING_STREAK: usize = 3;

fn created_by_schedule(task: &Task, schedule_id: &str) -> bool {
    task.created_by.as_deref() == Some(&format!("schedule:{schedule_id}"))
}

/// Whether a task from this schedule is still in `tasks/` (pending or running).
fn has_queued_run(home: &Home, schedule_id: &str) -> bool {
    md_files(&home.tasks())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| Task::load(p).ok())
        .any(|t| created_by_schedule(&t, schedule_id))
}

/// How many of the schedule's most recent finished runs failed in a row.
pub fn failure_streak(home: &Home, schedule_id: &str) -> usize {
    let mut runs: Vec<Task> = md_files(&home.archive())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| Task::load(p).ok())
        .filter(|t| created_by_schedule(t, schedule_id))
        .collect();
    runs.sort_by_key(|t| std::cmp::Reverse(t.finished_at));
    runs.iter()
        .take_while(|t| t.status == Status::Failed)
        .count()
}

fn truncate_id(id: &str, max: usize) -> &str {
    id[..id.len().min(max)].trim_end_matches('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_ids_without_trailing_dash() {
        assert_eq!(truncate_id("nightly-deps", 48), "nightly-deps");
        assert_eq!(truncate_id("abc-def", 4), "abc");
    }
}
