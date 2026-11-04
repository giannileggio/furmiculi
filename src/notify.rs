//! Notifications when a task ends: ntfy and Telegram.
//!
//! Configured by `[notify]` in config.toml; nothing is sent unless `on` is set
//! and a service is configured. Sending never affects a task: it runs in a
//! detached `curl` with a short timeout, and every error is logged and
//! ignored. Secrets come from files (or environment variables), never from
//! config.toml or a task, and reach `curl` on its stdin, not its argv, so
//! they don't show up in `ps`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{Config, Web, on_path};
use crate::home::{Home, expand_tilde};
use crate::run::log;
use crate::task::{Status, Task};

/// Env var read when `[notify.ntfy]` has no `token_file`.
pub const NTFY_TOKEN_ENV: &str = "FURMI_NTFY_TOKEN";
/// Env var read when `[notify.telegram]` has no `bot_token_file`.
pub const TELEGRAM_TOKEN_ENV: &str = "FURMI_TELEGRAM_BOT_TOKEN";

/// How long one send may take, in seconds, connecting included.
const MAX_SECONDS: u32 = 5;
/// Longest error text put in a message.
const MAX_ERROR_CHARS: usize = 500;

/// A kind of ending worth a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// The task is done.
    Finished,
    /// The task failed for good (no retries left).
    Failed,
    /// An interactive session ended without saying how it went.
    NeedsReview,
}

impl Event {
    fn of(status: Status) -> Option<Self> {
        match status {
            Status::Done => Some(Event::Finished),
            Status::Failed => Some(Event::Failed),
            Status::NeedsReview => Some(Event::NeedsReview),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notify {
    /// Which endings to send. Empty (the default): nothing is sent.
    pub on: Vec<Event>,
    pub ntfy: Option<Ntfy>,
    pub telegram: Option<Telegram>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ntfy {
    /// The server (`https://ntfy.sh`), or the full topic URL.
    pub url: Option<String>,
    /// Appended to `url` as its path, if `url` has none.
    pub topic: Option<String>,
    /// File holding an access token, for a protected topic.
    pub token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Telegram {
    /// File holding the bot token from @BotFather.
    pub bot_token_file: Option<PathBuf>,
    pub chat_id: Option<String>,
}

/// One HTTP POST, ready to send. `url` and `headers` may hold secrets.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<String>,
    pub body: String,
}

/// Sends requests. A trait so tests can stub it.
pub trait Sender {
    fn send(&self, request: &Request) -> Result<()>;
}

/// Sends with a detached `curl`: the caller never waits for the network.
struct Curl;

impl Sender for Curl {
    fn send(&self, request: &Request) -> Result<()> {
        let mut child = Command::new("curl")
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting curl")?;
        let config = curl_config(request);
        let mut stdin = child.stdin.take().context("curl's stdin")?;
        // A small write into a pipe: it can't block.
        let wrote = stdin.write_all(config.as_bytes());
        drop(stdin);
        // Reap it in the background, and say if it failed.
        std::thread::spawn(move || match child.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => log(&format!("notification not delivered: curl {status}")),
            Err(e) => log(&format!("notification not delivered: {e}")),
        });
        wrote.context("writing curl's config")
    }
}

/// A curl config file (read from stdin) that makes `request`.
fn curl_config(request: &Request) -> String {
    let q = curl_quote;
    let mut out = format!(
        "silent\nfail\nmax-time = {MAX_SECONDS}\nconnect-timeout = 3\nurl = {}\n",
        q(&request.url)
    );
    for header in &request.headers {
        out.push_str(&format!("header = {}\n", q(header)));
    }
    out.push_str(&format!("data = {}\n", q(&request.body)));
    out
}

/// `s` as a quoted curl-config string.
fn curl_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What a notification says about a task.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub title: String,
    pub body: String,
}

/// `link` is the dashboard's address for this task, if one is configured.
pub fn message(task: &Task, link: Option<&str>) -> Message {
    let status = match task.status {
        Status::Done => "finished",
        Status::Failed => "failed",
        Status::NeedsReview => "needs review",
        s => s.as_str(),
    };
    let mut body = format!(
        "Task: {}\nRepo: {}\nStatus: {status}\nAttempts: {}",
        task.id,
        task.spec.repo.display(),
        task.attempts
    );
    if let Some(error) = task.error.as_deref().filter(|e| !e.trim().is_empty()) {
        body.push_str("\nError: ");
        body.push_str(&truncate(error.trim(), MAX_ERROR_CHARS));
    }
    if let Some(link) = link {
        body.push('\n');
        body.push_str(link);
    }
    Message {
        title: format!("furmi: {} {status}", task.id),
        body,
    }
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// The dashboard's page for `id`, when `[web]` serves other machines under a
/// name (a loopback-only dashboard is no use from a phone).
pub fn dashboard_link(web: &Web, id: &str) -> Option<String> {
    let listen = web.listen.as_deref()?;
    if crate::web::is_loopback_authority(listen) {
        return None;
    }
    let host = web
        .allowed_hosts
        .iter()
        .map(|h| h.trim())
        .find(|h| !h.is_empty() && *h != "*")?;
    Some(format!("{}/tasks/{id}", crate::web::url_for(host, listen)))
}

/// Reads a secret from `file`, or from the environment variable `env`.
fn secret(file: Option<&Path>, env: &str) -> Result<Option<String>> {
    if let Some(file) = file {
        let path = expand_tilde(file);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        return Ok(Some(text.trim().to_string()).filter(|s| !s.is_empty()));
    }
    Ok(std::env::var(env)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

impl Ntfy {
    fn endpoint(&self) -> Result<String> {
        let url = self.url.as_deref().map(str::trim).unwrap_or_default();
        if url.is_empty() {
            bail!("[notify.ntfy] needs a url");
        }
        // A path after the host means the url already names the topic.
        let has_path = url
            .split_once("://")
            .map_or(url, |(_, rest)| rest)
            .trim_end_matches('/')
            .contains('/');
        match self
            .topic
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(topic) => Ok(format!(
                "{}/{}",
                url.trim_end_matches('/'),
                topic.trim_matches('/')
            )),
            None if has_path => Ok(url.to_string()),
            None => bail!("[notify.ntfy] needs a topic (or a url that ends in the topic)"),
        }
    }

    fn request(&self, msg: &Message) -> Result<Request> {
        let mut headers = vec![format!("Title: {}", ascii(&msg.title))];
        if let Some(token) = secret(self.token_file.as_deref(), NTFY_TOKEN_ENV)? {
            headers.push(format!("Authorization: Bearer {token}"));
        }
        Ok(Request {
            url: self.endpoint()?,
            headers,
            body: msg.body.clone(),
        })
    }
}

/// HTTP header values are ASCII.
fn ascii(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii() && !c.is_control() {
                c
            } else {
                '?'
            }
        })
        .collect()
}

impl Telegram {
    fn request(&self, msg: &Message) -> Result<Request> {
        let Some(chat_id) = self.chat_id.as_deref().filter(|c| !c.trim().is_empty()) else {
            bail!("[notify.telegram] needs a chat_id");
        };
        let Some(token) = secret(self.bot_token_file.as_deref(), TELEGRAM_TOKEN_ENV)? else {
            bail!("[notify.telegram] needs a bot_token_file (or {TELEGRAM_TOKEN_ENV})");
        };
        let body = serde_json::json!({
            "chat_id": chat_id.trim(),
            "text": format!("{}\n\n{}", msg.title, msg.body),
            "disable_web_page_preview": true,
        });
        Ok(Request {
            url: format!("https://api.telegram.org/bot{token}/sendMessage"),
            headers: vec!["Content-Type: application/json".into()],
            body: body.to_string(),
        })
    }
}

/// Sends `msg` to every configured service. Never fails: each error is logged.
pub fn dispatch(notify: &Notify, msg: &Message, sender: &dyn Sender) {
    let mut requests = Vec::new();
    if let Some(ntfy) = &notify.ntfy {
        requests.push(("ntfy", ntfy.request(msg)));
    }
    if let Some(telegram) = &notify.telegram {
        requests.push(("telegram", telegram.request(msg)));
    }
    for (name, request) in requests {
        if let Err(e) = request.and_then(|r| sender.send(&r)) {
            log(&format!("notification to {name} failed: {e:#}"));
        }
    }
}

/// Called when a task is archived: sends the message if `[notify]` asks for
/// this ending. Safe to call anywhere; it only logs.
pub fn task_archived(home: &Home, task: &Task) {
    let Ok(config) = Config::load(&home.config_path()) else {
        return;
    };
    notify_with(&config, task, &Curl);
}

fn notify_with(config: &Config, task: &Task, sender: &dyn Sender) {
    let Some(event) = Event::of(task.status) else {
        return;
    };
    if !config.notify.on.contains(&event) {
        return;
    }
    let link = dashboard_link(&config.web, &task.id);
    dispatch(&config.notify, &message(task, link.as_deref()), sender);
}

/// Problems with `[notify]`, for `furmi doctor`. Empty when it's fine or when
/// notifications aren't in use.
pub fn problems(notify: &Notify) -> Vec<String> {
    problems_with(notify, on_path("curl"))
}

fn problems_with(notify: &Notify, curl_installed: bool) -> Vec<String> {
    let mut out = Vec::new();
    if !curl_installed && (notify.ntfy.is_some() || notify.telegram.is_some()) {
        out.push("curl is not on $PATH: notifications can't be sent without it".into());
    }
    let probe = Message {
        title: String::new(),
        body: String::new(),
    };
    if notify.on.is_empty() {
        if notify.ntfy.is_some() || notify.telegram.is_some() {
            out.push("[notify] has a service but `on` is empty, so nothing is sent".into());
        }
        return out;
    }
    if notify.ntfy.is_none() && notify.telegram.is_none() {
        out.push("[notify] `on` is set but neither [notify.ntfy] nor [notify.telegram] is".into());
    }
    if let Some(ntfy) = &notify.ntfy
        && let Err(e) = ntfy.request(&probe)
    {
        out.push(format!("{e:#}"));
    }
    if let Some(telegram) = &notify.telegram
        && let Err(e) = telegram.request(&probe)
    {
        out.push(format!("{e:#}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn task(status: Status) -> Task {
        let mut t = Task::parse("---\nid: t1\nrepo: /src/app\n---\ndo it\n").unwrap();
        t.status = status;
        t.attempts = 2;
        t.error = Some("boom".into());
        t
    }

    #[derive(Default)]
    struct Stub(RefCell<Vec<Request>>);
    impl Sender for Stub {
        fn send(&self, r: &Request) -> Result<()> {
            self.0.borrow_mut().push(r.clone());
            Ok(())
        }
    }
    struct Broken;
    impl Sender for Broken {
        fn send(&self, _: &Request) -> Result<()> {
            bail!("network down")
        }
    }

    fn config(toml: &str) -> Config {
        Config::parse(toml).unwrap()
    }

    #[test]
    fn message_carries_id_repo_status_attempts_error_and_link() {
        let m = message(&task(Status::Failed), Some("http://box:7433/tasks/t1"));
        assert_eq!(m.title, "furmi: t1 failed");
        assert_eq!(
            m.body,
            "Task: t1\nRepo: /src/app\nStatus: failed\nAttempts: 2\nError: boom\nhttp://box:7433/tasks/t1"
        );
    }

    #[test]
    fn long_errors_are_cut() {
        let mut t = task(Status::Failed);
        t.error = Some("x".repeat(2000));
        assert!(message(&t, None).body.chars().count() < 600);
    }

    #[test]
    fn link_needs_a_reachable_dashboard() {
        let mut web = Web::default();
        assert_eq!(dashboard_link(&web, "t1"), None);
        web.listen = Some("0.0.0.0:7433".into());
        assert_eq!(dashboard_link(&web, "t1"), None);
        web.allowed_hosts = vec!["*".into(), "box.ts.net".into()];
        assert_eq!(
            dashboard_link(&web, "t1").as_deref(),
            Some("http://box.ts.net:7433/tasks/t1")
        );
    }

    #[test]
    fn sends_only_the_configured_endings() {
        let c = config(
            "[notify]\non = [\"failed\"]\n[notify.ntfy]\nurl = \"https://ntfy.sh\"\ntopic = \"furmi\"\n",
        );
        let stub = Stub::default();
        notify_with(&c, &task(Status::Done), &stub);
        notify_with(&c, &task(Status::Cancelled), &stub);
        assert!(stub.0.borrow().is_empty());
        notify_with(&c, &task(Status::Failed), &stub);
        let sent = stub.0.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].url, "https://ntfy.sh/furmi");
        assert!(sent[0].body.contains("Task: t1"));
    }

    #[test]
    fn nothing_is_sent_by_default() {
        let c = config("[notify.ntfy]\nurl = \"https://ntfy.sh/x\"\n");
        let stub = Stub::default();
        notify_with(&c, &task(Status::Failed), &stub);
        assert!(stub.0.borrow().is_empty());
    }

    #[test]
    fn a_failing_sender_changes_nothing() {
        let c = config("[notify]\non = [\"failed\"]\n[notify.ntfy]\nurl = \"https://ntfy.sh/x\"\n");
        let t = task(Status::Failed);
        let before = t.clone();
        notify_with(&c, &t, &Broken); // returns, doesn't panic
        assert_eq!(t, before);
    }

    #[test]
    fn telegram_reads_its_token_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tok");
        std::fs::write(&file, "123:abc\n").unwrap();
        let tg = Telegram {
            bot_token_file: Some(file),
            chat_id: Some("42".into()),
        };
        let r = tg.request(&message(&task(Status::Done), None)).unwrap();
        assert_eq!(r.url, "https://api.telegram.org/bot123:abc/sendMessage");
        assert!(r.body.contains("\"chat_id\":\"42\""));
    }

    #[test]
    fn ntfy_topic_and_token() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tok");
        std::fs::write(&file, "tk_1\n").unwrap();
        let n = Ntfy {
            url: Some("https://ntfy.example/".into()),
            topic: Some("alerts".into()),
            token_file: Some(file),
        };
        let r = n.request(&message(&task(Status::Done), None)).unwrap();
        assert_eq!(r.url, "https://ntfy.example/alerts");
        assert!(
            r.headers
                .contains(&"Authorization: Bearer tk_1".to_string())
        );
        assert!(
            Ntfy {
                url: Some("https://ntfy.sh".into()),
                ..Default::default()
            }
            .endpoint()
            .is_err()
        );
        assert!(
            Ntfy {
                url: Some("https://ntfy.sh/t".into()),
                ..Default::default()
            }
            .endpoint()
            .is_ok()
        );
    }

    #[test]
    fn curl_config_escapes_and_keeps_secrets_off_argv() {
        let r = Request {
            url: "https://x/y".into(),
            headers: vec!["A: b".into()],
            body: "a \"b\"\nc\\d".into(),
        };
        let c = curl_config(&r);
        assert!(c.contains("data = \"a \\\"b\\\"\\nc\\\\d\"\n"));
        assert!(c.contains("max-time = 5"));
    }

    #[test]
    fn doctor_problems() {
        assert!(problems_with(&Notify::default(), true).is_empty());
        let on_only = Notify {
            on: vec![Event::Failed],
            ..Default::default()
        };
        assert_eq!(problems_with(&on_only, true).len(), 1);
        let bad = Notify {
            on: vec![Event::Failed],
            telegram: Some(Telegram::default()),
            ..Default::default()
        };
        assert_eq!(problems_with(&bad, true).len(), 1);
    }

    #[test]
    fn doctor_reports_missing_curl_only_when_a_service_is_configured() {
        assert!(problems_with(&Notify::default(), false).is_empty());
        let ntfy = Ntfy {
            url: Some("https://ntfy.sh/x".into()),
            ..Default::default()
        };
        let configured = Notify {
            on: vec![Event::Failed],
            ntfy: Some(ntfy),
            ..Default::default()
        };
        assert!(problems_with(&configured, true).is_empty());
        let problems = problems_with(&configured, false);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("curl"));
    }
}
