//! `GET /api/events`: the dashboard's state, pushed as Server-Sent Events.
//!
//! The page opens one stream and redraws when a message arrives, instead of
//! asking `/api/state` every couple of seconds. The files behind the state
//! are changed by other processes (the runner, the CLI), so the server still
//! looks, but once a second and on its own side: a message goes out only when
//! the state differs from the last one sent, and an idle dashboard costs the
//! network a comment line every [`KEEPALIVE`].

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tiny_http::Request;

use super::Ctx;
use super::handlers::{json_err, json_log_with, tail_of};
use super::routes::query_param;
use super::state::state_json;

use crate::control;
use crate::run;
use crate::task::Status;

const CHECK_EVERY: Duration = Duration::from_secs(1);

/// A comment line now and then, so a proxy or the browser doesn't decide a
/// quiet stream has died.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Streams each hold a thread for as long as the page is open; this many is
/// plenty for a dashboard and keeps a runaway client from spawning more.
const MAX_STREAMS: usize = 32;

static OPEN_STREAMS: AtomicUsize = AtomicUsize::new(0);

/// Decrements [`OPEN_STREAMS`] when the stream ends, however it ends.
pub(super) struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        OPEN_STREAMS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Reserves a stream slot, or `None` if they are all taken.
pub(super) fn claim_slot() -> Option<Slot> {
    if OPEN_STREAMS.fetch_add(1, Ordering::SeqCst) >= MAX_STREAMS {
        OPEN_STREAMS.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    Some(Slot)
}

/// The state with the one field that changes on every call taken out, so two
/// snapshots compare equal when nothing the page shows has changed.
fn fingerprint(json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(mut v) => {
            if let Some(o) = v.as_object_mut() {
                o.remove("generated_at");
            }
            v.to_string()
        }
        Err(_) => json.to_string(),
    }
}

/// What one look at the thing being streamed found.
enum Step {
    /// The current picture. `key` says whether it differs from the last one
    /// sent; `json` is what goes out when it does.
    Snapshot { key: String, json: String },
    /// A last frame, then the stream is closed.
    Last(String),
    /// Nothing to say this time.
    Skip,
}

/// Which stream a URL asks for.
pub(super) enum Kind {
    State,
    Log {
        id: String,
        attempt: Option<u32>,
        tail: Option<usize>,
    },
}

/// `/api/events` (the state) or `/api/tasks/<id>/logs/stream` (a task's log),
/// if that is what `url` is.
pub(super) fn kind_of(url: &str) -> Option<Kind> {
    let path = url.split('?').next().unwrap_or("");
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["api", "events"] => Some(Kind::State),
        ["api", "tasks", id, "logs", "stream"] => Some(Kind::Log {
            id: (*id).to_string(),
            attempt: query_param(url, "attempt").and_then(|v| v.parse().ok()),
            tail: query_param(url, "tail").and_then(|v| v.parse().ok()),
        }),
        _ => None,
    }
}

pub(super) fn serve(ctx: &Ctx, request: Request, slot: Slot, kind: Kind) {
    match kind {
        Kind::State => state_stream(ctx, request, slot),
        Kind::Log { id, attempt, tail } => log_stream(ctx, request, slot, &id, attempt, tail),
    }
}

/// The dashboard's state, until the dashboard stops.
fn state_stream(ctx: &Ctx, request: Request, slot: Slot) {
    // Resent every KEEPALIVE even when nothing changed: the page shows when
    // it was generated and filters by age against the clock.
    stream(request, slot, true, || {
        match state_json(ctx.home, ctx.cwd_repo.as_deref()) {
            Ok(json) => Step::Snapshot {
                key: fingerprint(&json),
                json,
            },
            Err(_) => Step::Skip,
        }
    });
}

/// The end of a task's log, as `{log, truncated}` like the plain logs
/// endpoint. A task that isn't running has nothing more to say: its last
/// snapshot is followed by an `end` event. A log that can't be read ends the
/// stream with a `failed` event carrying the error.
fn log_stream(
    ctx: &Ctx,
    request: Request,
    slot: Slot,
    id: &str,
    attempt: Option<u32>,
    tail: Option<usize>,
) {
    stream(request, slot, false, || {
        let running = control::find(ctx.home, id).is_ok_and(|(_, t)| t.status == Status::Running);
        let log = match control::logs(ctx.home, id, attempt) {
            Ok(log) => log,
            // A running task's log can be briefly unreadable (its window is
            // gone but the task isn't finished yet, or it is just starting):
            // try again rather than end the stream.
            Err(_) if running => return Step::Skip,
            Err(e) => return Step::Last(format!("event: failed\ndata: {}\n\n", json_err(&e))),
        };
        let (log, cut) = match tail {
            Some(limit) => tail_of(&log, limit),
            None => (log, false),
        };
        let json = json_log_with(&log, cut);
        if running {
            Step::Snapshot {
                key: json.clone(),
                json,
            }
        } else {
            Step::Last(format!("data: {json}\n\nevent: end\ndata: {{}}\n\n"))
        }
    });
}

/// Serves a stream on this thread until the client goes away, `look` says it
/// is over, or the dashboard is stopped. With `heartbeat`, the snapshot is
/// resent when it has been quiet for [`KEEPALIVE`]; otherwise a comment is.
fn stream(request: Request, _slot: Slot, heartbeat: bool, mut look: impl FnMut() -> Step) {
    let mut out = request.into_writer();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
                X-Accel-Buffering: no\r\nConnection: close\r\n\r\n";
    if out
        .write_all(head.as_bytes())
        .and_then(|_| out.flush())
        .is_err()
    {
        return;
    }
    let mut last = String::new();
    let mut last_write = Instant::now();
    while !run::shutdown_requested() {
        let message = match look() {
            Step::Snapshot { key, json } => {
                if key == last {
                    (heartbeat && last_write.elapsed() >= KEEPALIVE)
                        .then(|| format!("data: {json}\n\n"))
                } else {
                    last = key;
                    Some(format!("data: {json}\n\n"))
                }
            }
            Step::Last(frame) => {
                let _ = out.write_all(frame.as_bytes()).and_then(|_| out.flush());
                return;
            }
            Step::Skip => None,
        };
        let chunk = match message {
            Some(m) => Some(m),
            None if last_write.elapsed() >= KEEPALIVE => Some(": keepalive\n\n".to_string()),
            None => None,
        };
        if let Some(chunk) = chunk {
            if out
                .write_all(chunk.as_bytes())
                .and_then(|_| out.flush())
                .is_err()
            {
                return;
            }
            last_write = Instant::now();
        }
        std::thread::sleep(CHECK_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_of_recognises_the_two_streams() {
        assert!(matches!(kind_of("/api/events"), Some(Kind::State)));
        match kind_of("/api/tasks/t1/logs/stream?attempt=2&tail=500") {
            Some(Kind::Log { id, attempt, tail }) => {
                assert_eq!((id.as_str(), attempt, tail), ("t1", Some(2), Some(500)));
            }
            _ => panic!("not a log stream"),
        }
        assert!(kind_of("/api/tasks/t1/logs").is_none());
        assert!(kind_of("/api/state").is_none());
    }

    #[test]
    fn fingerprint_ignores_generated_at_only() {
        let a = r#"{"generated_at":"1","queue":[]}"#;
        let b = r#"{"generated_at":"2","queue":[]}"#;
        let c = r#"{"generated_at":"2","queue":[1]}"#;
        assert_eq!(fingerprint(a), fingerprint(b));
        assert_ne!(fingerprint(a), fingerprint(c));
    }
}
