//! The token usage an agent reported, and the failure it reported in its
//! event stream: both parsed from one attempt's headless stdout.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Token usage an agent reported for one attempt, and how the agent said
/// it went, both parsed from its headless stdout.
///
/// Two shapes are understood, since that is what agents print:
///
/// - one final JSON object with a `usage` field (Claude Code's
///   `--output-format json`);
/// - an event stream, one JSON object per line, where each finished assistant
///   message reports its own usage (pi's `--mode json`). Those are summed, and
///   an error/aborted stop reason is picked up as well: that mode exits 0 even
///   when the run failed.
///
/// Any other agent (plain text, or nothing printed yet) reports nothing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tokens_out: Option<u64>,
    /// The model the agent said it ran with (`provider/model` when it names a
    /// provider too). A task that pins no model still runs with one, and only
    /// the agent's own output knows which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
}

/// What one attempt's headless stdout said: the usage it reported and, when
/// the stream reports outcomes as events, the error it ended on.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Stream {
    pub(super) usage: Option<Usage>,
    /// Why the agent said the run failed (its own words). `None` unless it
    /// reported one, which for an event stream is not a non-zero exit.
    pub(super) error: Option<String>,
}

impl Stream {
    pub(super) fn parse(bytes: &[u8]) -> Stream {
        let single = serde_json::from_slice::<serde_json::Value>(bytes).ok();
        // One object for the whole stdout with a `usage` in it is how Claude
        // Code reports. Anything else that parses as one object is simply a
        // stream of one event, and is read like every other event below.
        if let Some(value) = single.as_ref()
            && let Some(usage) = Usage::from_claude(value)
        {
            return Stream {
                usage: Some(usage),
                error: None,
            };
        }
        let mut stream = Stream::default();
        match single {
            Some(event) => stream.read_event(&event),
            None => {
                // One object per line, as pi's `--mode json` writes them; any
                // other line (a warning, a plain-text agent) is skipped.
                for line in bytes.split(|b| *b == b'\n') {
                    if let Ok(event) = serde_json::from_slice::<serde_json::Value>(line) {
                        stream.read_event(&event);
                    }
                }
            }
        }
        stream
    }

    /// Folds one event of an agent's JSON stream into the totals: the usage of
    /// a finished assistant message, and how the run ended. Only the final
    /// outcome counts: pi retries a transient failure in-process, so an error
    /// in a message that a later successful retry (or a later clean message)
    /// recovered from must not fail the run.
    fn read_event(&mut self, event: &serde_json::Value) {
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("message_end") => {}
            Some("auto_retry_end") => {
                // A retry that succeeded means the error it was retrying is
                // over, even if no later assistant message says so.
                if event.get("success").and_then(serde_json::Value::as_bool) == Some(true) {
                    self.error = None;
                }
                return;
            }
            // `agent_end` and friends repeat what `message_end` said.
            _ => return,
        }
        let Some(message) = event.get("message") else {
            return;
        };
        if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
            return;
        }
        if let Some(usage) = Usage::from_pi(message) {
            self.usage = Some(match self.usage.take() {
                Some(total) => total.add(usage),
                None => usage,
            });
        }
        // A message that ended cleanly after a failed one means the run
        // recovered; a genuinely failed run ends on the error message.
        self.error = pi_error(message);
    }
}

impl Usage {
    /// Usage as Claude Code reports it: a `usage` object.
    fn from_claude(value: &serde_json::Value) -> Option<Usage> {
        let usage = value.get("usage")?;
        let tokens = |key: &str| usage.get(key).and_then(serde_json::Value::as_u64);
        let tokens_in = tokens("input_tokens").map(|n| {
            n + tokens("cache_creation_input_tokens").unwrap_or(0)
                + tokens("cache_read_input_tokens").unwrap_or(0)
        });
        let tokens_out = tokens("output_tokens");
        let model = value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        Some(Usage {
            tokens_in,
            tokens_out,
            model,
        })
        .filter(Usage::reported)
    }

    /// Usage as one of pi's assistant messages reports it: `input`, `output`,
    /// and the cache counters.
    fn from_pi(message: &serde_json::Value) -> Option<Usage> {
        // `usage` can be missing (a message cut short before it reported any),
        // but the model it names is still worth keeping.
        let usage = message.get("usage");
        let count = |key: &str| {
            usage
                .and_then(|u| u.get(key))
                .and_then(serde_json::Value::as_u64)
        };
        let tokens_in = count("input")
            .map(|n| n + count("cacheRead").unwrap_or(0) + count("cacheWrite").unwrap_or(0));
        let tokens_out = count("output");
        // pi names the model on every assistant message, and the provider
        // alongside it: the same `provider/model` a task's `model:` field
        // takes, so the two read alike.
        let model = message
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(
                |model| match message.get("provider").and_then(serde_json::Value::as_str) {
                    Some(provider) => format!("{provider}/{model}"),
                    None => model.to_string(),
                },
            );
        Some(Usage {
            tokens_in,
            tokens_out,
            model,
        })
        .filter(Usage::reported)
    }

    /// Did anything in here report something worth writing down?
    fn reported(&self) -> bool {
        self.tokens_in.is_some() || self.tokens_out.is_some() || self.model.is_some()
    }

    /// One turn plus another, field by field. The model isn't a total: the
    /// first answer stands (a run that switches model mid-way still ran as the
    /// one it started with, and a broken retry reports nothing at all).
    fn add(self, other: Usage) -> Usage {
        Usage {
            tokens_in: plus(self.tokens_in, other.tokens_in),
            tokens_out: plus(self.tokens_out, other.tokens_out),
            model: self.model.or(other.model),
        }
    }

    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(&self).expect("Usage always serializes")
    }
}

fn plus<T: std::ops::Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Why pi's assistant message ended badly, if it did: the reason is only in
/// the event stream, since `--mode json` exits 0 either way.
fn pi_error(message: &serde_json::Value) -> Option<String> {
    let stop = message
        .get("stopReason")
        .and_then(serde_json::Value::as_str)?;
    if !matches!(stop, "error" | "aborted") {
        return None;
    }
    let said = message
        .get("errorMessage")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty());
    Some(match said {
        Some(said) => format!("the agent reported {stop}: {said}"),
        None => format!("the agent reported {stop}"),
    })
}

/// The model a saved attempt's log says it ran with.
///
/// For a task whose file records none: its run happened before `model_used` was
/// kept, and the log is the only place left that knows. Reads the head of the
/// log only — the first assistant message names the model, and a log can be
/// 64 MB — and lets the cut-off last line fail to parse, like any other line
/// that isn't an event.
pub(crate) fn model_from_log(path: &Path) -> Option<String> {
    const HEAD: u64 = 256 * 1024;
    let file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    std::io::Read::take(file, HEAD)
        .read_to_end(&mut head)
        .ok()?;
    Stream::parse(&head).usage?.model
}

pub(super) fn read_usage(path: &Path) -> Option<Usage> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::exec::{Exit, recorded_exit};

    #[test]
    fn parses_claude_code_usage() {
        let stdout = r#"{"type":"result","total_cost_usd":0.0512,"model":"claude-sonnet-4-5-20250929","usage":{"input_tokens":120,"cache_creation_input_tokens":30,"cache_read_input_tokens":500,"output_tokens":80}}"#;
        let stream = Stream::parse(stdout.as_bytes());
        let usage = stream.usage.unwrap();
        assert_eq!(usage.tokens_in, Some(120 + 30 + 500));
        assert_eq!(usage.tokens_out, Some(80));
        assert_eq!(usage.model.as_deref(), Some("claude-sonnet-4-5-20250929"));
        assert_eq!(stream.error, None);
    }
    #[test]
    fn records_the_model_a_pi_stream_says_it_ran_with() {
        // A task that pins no `model` still runs with one, and pi's assistant
        // messages are the only place that says which.
        let stdout = [
            r#"{"type":"message_end","message":{"role":"assistant","provider":"opencode-go","model":"deepseek-v4.1-flash","usage":{"input":10}}}"#,
            // A second turn can name a different model; the first one is the
            // one the run started with.
            r#"{"type":"message_end","message":{"role":"assistant","provider":"opencode-go","model":"something-else","usage":{"input":10}}}"#,
        ]
        .join("\n");
        let usage = Stream::parse(stdout.as_bytes()).usage.unwrap();
        assert_eq!(usage.tokens_in, Some(20));
        assert_eq!(
            usage.model.as_deref(),
            Some("opencode-go/deepseek-v4.1-flash")
        );

        // A model with no provider named is kept as it came.
        let bare = r#"{"type":"message_end","message":{"role":"assistant","model":"bare","usage":{"input":1}}}"#;
        assert_eq!(
            Stream::parse(bare.as_bytes()).usage.unwrap().model,
            Some("bare".into())
        );

        // A model on its own, with no usage numbers, is still worth writing
        // down: the run's model is the whole point.
        let model_only = r#"{"type":"message_end","message":{"role":"assistant","model":"m"}}"#;
        let usage = Stream::parse(model_only.as_bytes()).usage.unwrap();
        assert_eq!(usage.tokens_in, None);
        assert_eq!(usage.model, Some("m".into()));
    }
    #[test]
    fn the_first_model_a_stream_reports_wins_over_a_later_one() {
        let usage = Usage {
            model: Some("first".into()),
            ..Usage::default()
        };
        let later = Usage {
            model: Some("second".into()),
            tokens_in: Some(5),
            ..Usage::default()
        };
        let total = usage.add(later);
        assert_eq!(total.model.as_deref(), Some("first"));
        assert_eq!(total.tokens_in, Some(5));
        // Nothing to override it with: the first answer stands.
        let total = total.add(Usage::default());
        assert_eq!(total.model.as_deref(), Some("first"));
    }
    #[test]
    fn parses_pi_json_events_by_summing_turns() {
        // Two turns, each reporting its own usage; the surrounding events
        // (and a non-JSON line) must not add anything or trip it up.
        let stdout = [
            r#"{"type":"session","version":3,"id":"s"}"#,
            r#"{"type":"agent_start"}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"toolUse","usage":{"input":100,"output":10,"cacheRead":5,"cacheWrite":2,"totalTokens":117,"cost":{"total":0.01}}}}"#,
            "warning: something on stderr-ish stdout",
            r#"{"type":"message_end","message":{"role":"user","content":"and then?"}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop","usage":{"input":200,"output":20,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.02}}}}"#,
            // agent_end repeats the same messages; counting those would double.
            r#"{"type":"agent_end","messages":[{"role":"assistant","usage":{"input":100,"output":10}}]}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]
        .join("\n");
        let stream = Stream::parse(stdout.as_bytes());
        let usage = stream.usage.unwrap();
        assert_eq!(usage.tokens_in, Some((100 + 5 + 2) + 200));
        assert_eq!(usage.tokens_out, Some(30));
        assert_eq!(stream.error, None);
    }
    #[test]
    fn a_pi_stream_error_is_reported_even_though_the_exit_was_zero() {
        // pi's `--mode json` exits 0 for an aborted or errored response, so
        // the reason has to come out of the stream.
        let stdout = [
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"model is not available","usage":{"input":10,"output":0,"cost":{"total":0.001}}}}"#,
        ]
        .join("\n");
        let stream = Stream::parse(stdout.as_bytes());
        assert_eq!(
            stream.error.as_deref(),
            Some("the agent reported error: model is not available")
        );
        // The failed turn still reports what it spent.
        assert_eq!(stream.usage.unwrap().tokens_in, Some(10));

        // An abort with nothing said about why.
        let aborted =
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"aborted"}}"#;
        assert_eq!(
            Stream::parse(aborted.as_bytes()).error.as_deref(),
            Some("the agent reported aborted")
        );

        // A stream that ends normally says nothing about failing.
        let ok = r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}"#;
        let stream = Stream::parse(ok.as_bytes());
        assert_eq!(stream.error, None);
        assert_eq!(stream.usage, None);
    }
    #[test]
    fn a_retried_pi_error_does_not_fail_the_run() {
        // pi retries a transient provider failure in-process: when the retry
        // succeeds the run did not fail, so the earlier error must not stick.
        let retried = [
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"Request timed out.","usage":{"input":10,"output":0}}}"#,
            r#"{"type":"auto_retry_start","attempt":1,"maxAttempts":3}"#,
            r#"{"type":"auto_retry_end","attempt":1,"success":true}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop","usage":{"input":20,"output":5}}}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]
        .join("\n");
        let stream = Stream::parse(retried.as_bytes());
        assert_eq!(stream.error, None);
        // The run's exit stays 0, and both turns still report their usage.
        assert_eq!(recorded_exit(Exit::Code(0), &stream), Exit::Code(0));
        assert_eq!(stream.usage.unwrap().tokens_in, Some(30));

        // A successful retry clears the error even when no later assistant
        // message follows it, and a later clean message clears it too.
        let quiet_retry = [
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"Request timed out."}}"#,
            r#"{"type":"auto_retry_end","success":true}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]
        .join("\n");
        assert_eq!(Stream::parse(quiet_retry.as_bytes()).error, None);
        let later_clean = [
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"Request timed out."}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}"#,
        ]
        .join("\n");
        assert_eq!(Stream::parse(later_clean.as_bytes()).error, None);

        // A run that really did fail still ends up failed.
        let failed = [
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"model is not available"}}"#,
            r#"{"type":"auto_retry_end","success":false}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"model is not available"}}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]
        .join("\n");
        let stream = Stream::parse(failed.as_bytes());
        assert_eq!(
            stream.error.as_deref(),
            Some("the agent reported error: model is not available")
        );
        assert_eq!(recorded_exit(Exit::Code(0), &stream), Exit::Code(1));
    }
    #[test]
    fn no_usage_for_non_json_or_usage_less_output() {
        assert_eq!(
            Stream::parse(b"plain text output, not json"),
            Stream::default()
        );
        assert_eq!(
            Stream::parse(br#"{"type":"result","result":"ok"}"#),
            Stream::default()
        );
    }
    #[test]
    fn reads_the_model_out_of_a_saved_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.1.log");
        std::fs::write(
            &path,
            [
                r#"{"type":"session","version":3,"id":"s"}"#,
                r#"{"type":"message_end","message":{"role":"assistant","provider":"opencode-go","model":"deepseek-v4.1-flash","usage":{"input":10}}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        assert_eq!(
            model_from_log(&path).as_deref(),
            Some("opencode-go/deepseek-v4.1-flash")
        );

        // A log with no model in it, a log that isn't an event stream, and one
        // that isn't there at all say nothing rather than guessing.
        std::fs::write(&path, "just some output\n").unwrap();
        assert_eq!(model_from_log(&path), None);
        assert_eq!(model_from_log(&dir.path().join("missing.log")), None);
    }
    #[test]
    fn usage_roundtrips_through_json() {
        let usage = Usage {
            tokens_in: Some(650),
            tokens_out: Some(80),
            model: Some("opencode-go/deepseek-v4.1-flash".into()),
        };
        let read_back: Usage = serde_json::from_str(&usage.to_json()).unwrap();
        assert_eq!(usage, read_back);
    }
}
