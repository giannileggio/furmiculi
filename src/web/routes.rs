//! The dashboard's routes: one function that turns a method, path and body
//! into a [`Reply`], handing the work to the handlers in [`super::handlers`]
//! and the page's data to [`super::state`].

use tiny_http::Method;

use super::handlers::{
    agents_json, detail_reply, doctor_json, json_err, json_log_with, mutation_reply,
    perform_action, perform_create, perform_create_schedule, perform_edit_schedule,
    perform_edit_task, perform_set_default_agent, tail_of, task_view_json,
};
use super::state::{capacity, repos_json, state_json};
use super::{Ctx, Reply, json_reply, reply};

use crate::control;
use crate::edit;

const ACTIONS: [&str; 4] = ["done", "fail", "cancel", "retry"];

/// The ant from the README, cropped to the mark itself and carried inside the
/// binary: no file to install, no request off the box, and the same picture the
/// repository shows. Two of them, because the artwork's own colours are two:
/// dark brown for a light page, cream for a dark one.
const LOGO: &[u8] = include_bytes!("../../assets/logo-mark.png");
const LOGO_DARK: &[u8] = include_bytes!("../../assets/logo-mark-dark.png");

/// A created task's answer, saying which machine `auto` put it on (and, for
/// another machine, its id as the dashboard names it there).
fn named(answer: String, placed: Option<&str>) -> String {
    let Some(name) = placed else { return answer };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&answer) else {
        return answer;
    };
    if v["ok"] == true {
        v["node"] = name.into();
        if name != crate::nodes::LOCAL
            && let Some(id) = v["id"].as_str()
        {
            v["id"] = format!("{id}@{name}").into();
        }
    }
    v.to_string()
}

pub(super) fn route(ctx: &Ctx, method: &Method, url: &str, body: &str) -> Reply {
    let home = ctx.home;
    let path = url.split('?').next().unwrap_or("");
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // `node: auto` is settled here, into a named machine, before anything is sent.
    let settled;
    let mut placed = None;
    let body = if matches!(method, Method::Post) && segments == ["api", "tasks"] {
        match crate::nodes::resolve_auto(home, || capacity(home, ctx.cwd_repo.as_deref()), body) {
            Ok(Some((form, name))) => {
                settled = form;
                placed = Some(name);
                settled.as_str()
            }
            Ok(None) => body,
            Err(e) => return json_reply(409, json_err(&e)),
        }
    } else {
        body
    };
    // A row, or a form, that belongs to another node goes there.
    if let Some((status, answer)) =
        crate::nodes::proxy(home, matches!(method, Method::Post), url, body)
    {
        return json_reply(status, named(answer, placed.as_deref()));
    }
    match method {
        Method::Get => match segments.as_slice() {
            [] => reply(200, "text/html; charset=utf-8", ctx.index.clone()),
            ["logo.png"] => Reply {
                asset: Some(LOGO),
                ..reply(200, "image/png", "")
            },
            ["logo-dark.png"] => Reply {
                asset: Some(LOGO_DARK),
                ..reply(200, "image/png", "")
            },
            ["api", "state"] => match state_json(home, ctx.cwd_repo.as_deref()) {
                // `?local=1` is how a hub asks: this machine's own queue,
                // never a merge, so two hubs can't end up asking each other.
                Ok(json) if query_param(url, "local") == Some("1") => json_reply(200, json),
                Ok(json) => json_reply(200, crate::nodes::merge_state(home, json)),
                Err(e) => json_reply(500, json_err(&e)),
            },
            ["api", "repos"] => match repos_json(home, ctx.cwd_repo.as_deref()) {
                Ok(json) => json_reply(200, json),
                Err(e) => json_reply(500, json_err(&e)),
            },
            ["api", "doctor"] => json_reply(200, doctor_json(home)),
            ["api", "agents"] => match agents_json(home) {
                Ok(json) => json_reply(200, json),
                Err(e) => json_reply(500, json_err(&e)),
            },
            ["api", "tasks", id] => detail_reply(edit::task_detail(home, id)),
            ["api", "tasks", id, "view"] => match task_view_json(home, id) {
                Ok(json) => json_reply(200, json),
                Err(e) => json_reply(404, json_err(&e)),
            },
            ["api", "schedules", id] => detail_reply(edit::schedule_detail(home, id)),
            ["api", "tasks", id, "logs"] => {
                let attempt = query_param(url, "attempt").and_then(|v| v.parse().ok());
                match control::logs(home, id, attempt) {
                    Ok(log) => {
                        // `?tail=<bytes>`: the end of a long log, for the live
                        // view, instead of re-reading all of it every poll.
                        let (log, cut) = match query_param(url, "tail").and_then(|v| v.parse().ok())
                        {
                            Some(limit) => tail_of(&log, limit),
                            None => (log, false),
                        };
                        json_reply(200, json_log_with(&log, cut))
                    }
                    Err(e) => json_reply(404, json_err(&e)),
                }
            }
            _ => reply(404, "text/plain", "not found"),
        },
        Method::Post => match segments.as_slice() {
            ["api", "runner", "start"] => match control::start_runner(home) {
                Ok(started) => json_reply(
                    200,
                    serde_json::json!({ "ok": true, "started": started }).to_string(),
                ),
                Err(e) => json_reply(400, json_err(&e)),
            },
            ["api", "runner", "stop"] => match control::stop_runner(home) {
                Ok(pid) => json_reply(
                    200,
                    serde_json::json!({ "ok": true, "pid": pid }).to_string(),
                ),
                Err(e) => json_reply(400, json_err(&e)),
            },
            ["api", "tasks", id, "edit"] => mutation_reply(perform_edit_task(home, id, body)),
            ["api", "schedules"] => match perform_create_schedule(home, body) {
                Ok(id) => json_reply(200, serde_json::json!({ "ok": true, "id": id }).to_string()),
                Err(e) => json_reply(400, json_err(&e)),
            },
            ["api", "schedules", id, "edit"] => {
                mutation_reply(perform_edit_schedule(home, id, body))
            }
            ["api", "schedules", id, "pause"] => {
                mutation_reply(edit::set_schedule_enabled(home, id, false))
            }
            ["api", "schedules", id, "resume"] => {
                mutation_reply(edit::set_schedule_enabled(home, id, true))
            }
            ["api", "schedules", id, "delete"] => mutation_reply(edit::delete_schedule(home, id)),
            ["api", "agents", "default"] => mutation_reply(perform_set_default_agent(home, body)),
            ["api", "tasks"] => match perform_create(home, ctx.cwd_repo.as_deref(), body) {
                Ok(id) => json_reply(
                    200,
                    named(
                        serde_json::json!({ "ok": true, "id": id }).to_string(),
                        placed.as_deref(),
                    ),
                ),
                Err(e) => json_reply(400, json_err(&e)),
            },
            ["api", "tasks", id, action] if ACTIONS.contains(action) => {
                match perform_action(home, id, action, body) {
                    Ok(()) => json_reply(200, "{\"ok\":true}".into()),
                    Err(e) => json_reply(400, json_err(&e)),
                }
            }
            _ => reply(404, "text/plain", "not found"),
        },
        _ => reply(404, "text/plain", "not found"),
    }
}

pub(super) fn query_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_finds_and_misses() {
        assert_eq!(
            query_param("/api/tasks/x/logs?attempt=2", "attempt"),
            Some("2")
        );
        assert_eq!(query_param("/api/tasks/x/logs?a=1&b=2", "b"), Some("2"));
        assert_eq!(query_param("/api/tasks/x/logs", "attempt"), None);
        assert_eq!(query_param("/api/tasks/x/logs?a=1", "attempt"), None);
    }
}
