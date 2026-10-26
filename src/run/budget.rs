//! The runner's daily cost budget: what today's finished and running tasks
//! have cost so far, and whether that is over `runner.daily_budget_usd`.

use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, FixedOffset, Local};

use crate::cmd::md_files;
use crate::config::Config;
use crate::home::Home;
use crate::task::Task;

use super::log::log;

/// Cost agents reported for tasks run today (local time): finished ones in
/// `archive/` and the ones still queued or running in `tasks/`.
fn spent_today(home: &Home, now: DateTime<FixedOffset>) -> f64 {
    let midnight = now
        .with_timezone(&Local)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(Local).earliest())
        .map(|t| t.fixed_offset());
    let today = |t: &Task| match (midnight, t.finished_at.or(t.started_at)) {
        (Some(m), Some(at)) => at >= m,
        _ => false,
    };
    [home.tasks(), home.archive()]
        .iter()
        .flat_map(|dir| md_files(dir).unwrap_or_default())
        .filter_map(|p| Task::load(&p).ok())
        .filter(today)
        .filter_map(|t| t.cost_usd)
        .sum()
}

static BUDGET_NOTIFIED: AtomicBool = AtomicBool::new(false);

/// Whether `runner.daily_budget_usd` is used up. Logs once per day-crossing.
pub(super) fn over_budget(home: &Home, config: &Config, now: DateTime<FixedOffset>) -> bool {
    let Some(budget) = config.runner.daily_budget_usd else {
        return false;
    };
    let spent = spent_today(home, now);
    let over = spent >= budget;
    if over && !BUDGET_NOTIFIED.swap(true, Ordering::Relaxed) {
        log(&format!(
            "daily budget reached (${spent:.2} of ${budget:.2}): not starting new tasks until tomorrow"
        ));
    } else if !over {
        BUDGET_NOTIFIED.store(false, Ordering::Relaxed);
    }
    over
}
