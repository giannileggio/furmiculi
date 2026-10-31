//! Other machines running furmi, whose queues the dashboard shows next to this
//! one's.
//!
//! Each node is a self-contained furmi serving its own dashboard. The hub (the
//! machine you open) asks every node for its `/api/state`, tags the rows with
//! the node's name and merges them. Nothing is shared or synced: a node that
//! is down is shown as offline and its rows drop out, and everything else
//! keeps working.
//!
//! The list lives in `nodes.toml` beside `config.toml`, 0600 because it holds
//! each node's bearer token.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::home::Home;
use crate::spec::validate_id;

/// The name rows from this machine carry once other nodes are merged in.
pub const LOCAL: &str = "local";
/// How long a node gets, connecting and answering together, before it counts
/// as offline. One deadline for the whole request: a node that drips a byte at
/// a time must not hold a web worker for longer than this.
const TIMEOUT: Duration = Duration::from_secs(3);
/// How long an answer is reused, so a page polling every couple of seconds, or
/// several open pages, ask a node once.
const CACHE_TTL: Duration = Duration::from_secs(2);
/// A failure is reused for longer: a node that is down costs a timeout each
/// time it's asked, and a dead one shouldn't be asked on every poll.
const FAILURE_TTL: Duration = Duration::from_secs(10);
/// A change made on a node (a task action can run its when-done step) gets
/// longer than a read does.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// A node's state beyond this is refused rather than buffered.
const MAX_BODY: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub name: String,
    /// `http://host[:port]`, where that node's `furmi web` is served.
    pub url: String,
    /// The node's bearer token (its `~/.furmiculi/web-token`). Optional for a
    /// loopback address, e.g. an SSH tunnel to a node that only listens there.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodesFile {
    #[serde(default, rename = "node")]
    nodes: Vec<Node>,
}

fn path(home: &Home) -> PathBuf {
    home.root().join("nodes.toml")
}

pub fn load(home: &Home) -> Result<Vec<Node>> {
    let path = path(home);
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path)
            && meta.permissions().mode() & 0o077 != 0
        {
            bail!(
                "{} must be readable only by you (chmod 600): it holds tokens",
                path.display()
            );
        }
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: NodesFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(file.nodes)
}

fn save(home: &Home, nodes: Vec<Node>) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = path(home);
    let body = toml::to_string(&NodesFile { nodes })?;
    let text = format!("# Written by `furmi node`; holds each node's token.\n{body}");
    // Private from the first byte, not chmod'ed after the fact.
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

#[derive(Subcommand)]
pub enum NodeCmd {
    /// Show another machine's queue in this dashboard.
    ///
    /// URL is where that machine serves `furmi web` (see docs/REMOTE.md), and
    /// the token its `~/.furmiculi/web-token`.
    Add(AddArgs),
    /// List the nodes and whether they answer.
    Ls,
    /// Stop showing a node.
    Rm { name: String },
}

#[derive(Args)]
pub struct AddArgs {
    /// A short name for the node, shown beside its tasks (a-z, 0-9, `-`).
    name: String,
    /// e.g. http://box.tailnet.ts.net:7433
    url: String,
    /// Read the bearer token from this file (must be 0600).
    #[arg(long, conflicts_with = "token")]
    token_file: Option<PathBuf>,
    /// The bearer token itself (lands in shell history; prefer --token-file).
    #[arg(long)]
    token: Option<String>,
}

pub fn run(home: &Home, cmd: NodeCmd) -> Result<()> {
    match cmd {
        NodeCmd::Add(args) => add(home, args),
        NodeCmd::Ls => ls(home),
        NodeCmd::Rm { name } => {
            let _lock = home.lock()?;
            let mut nodes = load(home)?;
            let before = nodes.len();
            nodes.retain(|n| n.name != name);
            if nodes.len() == before {
                bail!("no node called `{name}`");
            }
            save(home, nodes)?;
            println!("removed {name}");
            Ok(())
        }
    }
}

fn add(home: &Home, args: AddArgs) -> Result<()> {
    let token = match (args.token, args.token_file) {
        (Some(t), _) => t.trim().to_string(),
        (None, Some(file)) => crate::web::auth::read_token_file(&file)?,
        (None, None) => String::new(),
    };
    let node = Node {
        name: args.name,
        url: args.url.trim().trim_end_matches('/').to_string(),
        token,
    };
    check(&node)?;
    {
        let _lock = home.lock()?;
        let mut nodes = load(home)?;
        // Same name: replaced, so adding again is how a token is changed.
        nodes.retain(|n| n.name != node.name);
        nodes.push(node.clone());
        save(home, nodes)?;
    }
    println!("added {}", node.name);
    if !node.token.is_empty() && !private_network(&parse_url(&node.url)?.0) {
        println!(
            "warning: {} isn't loopback or a tailnet address, and the token goes over plain \
             HTTP; reach it through a tailnet or an SSH tunnel",
            node.url
        );
    }
    match fetch(&node, "/api/state?local=1") {
        Ok(_) => println!("{} answers", node.url),
        Err(e) => println!("warning: {} doesn't answer yet: {e:#}", node.url),
    }
    Ok(())
}

fn ls(home: &Home) -> Result<()> {
    let nodes = load(home)?;
    if nodes.is_empty() {
        println!("no nodes; add one with `furmi node add <name> <url> --token-file <file>`");
        return Ok(());
    }
    let statuses: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = nodes
            .iter()
            .map(|n| {
                s.spawn(|| match fetch(n, "/api/state?local=1") {
                    Ok(_) => "online".to_string(),
                    Err(e) => format!("offline: {e:#}"),
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| "crashed".into()))
            .collect()
    });
    for (node, status) in nodes.iter().zip(statuses) {
        println!("{:<16} {:<40} {status}", node.name, node.url);
    }
    Ok(())
}

/// Whether a plain-HTTP token is travelling somewhere already encrypted or
/// private: loopback, or a tailnet (100.64.0.0/10, `*.ts.net`).
fn private_network(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".ts.net") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            ip.is_loopback() || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0]
        }
        Err(_) => false,
    }
}

fn check(node: &Node) -> Result<()> {
    validate_id(&node.name).map_err(anyhow::Error::msg)?;
    // Both go into the request's header lines as they are.
    if node
        .url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
        || node.token.chars().any(|c| !c.is_ascii_graphic())
    {
        bail!("the URL and token must be single words of printable characters");
    }
    if node.name == LOCAL {
        bail!("`{LOCAL}` is this machine's own name; pick another");
    }
    parse_url(&node.url)?;
    Ok(())
}

/// `(host, port, Host header)` of a plain-HTTP node URL.
fn parse_url(url: &str) -> Result<(String, u16, String)> {
    let rest = match url.strip_prefix("http://") {
        Some(rest) => rest,
        None if url.starts_with("https://") => bail!(
            "`{url}`: furmi's dashboard speaks plain HTTP, so use http:// (the tailnet or an \
             SSH tunnel already encrypts it)"
        ),
        None => bail!("`{url}` must look like http://host:7433"),
    };
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
        bail!("`{url}` must be just http://host[:port], with no path or credentials");
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !p.contains(']') => (h, p.parse().context("the port isn't a number")?),
        _ => (rest, 80),
    };
    Ok((
        host.trim_matches(['[', ']']).to_string(),
        port,
        rest.to_string(),
    ))
}

/// One GET to a node, returning the body of a 200.
fn fetch(node: &Node, path: &str) -> Result<String> {
    match exchange(node, false, path, "", TIMEOUT)? {
        (200, body) => Ok(body),
        (401, _) => bail!("refused the token (HTTP 401)"),
        (403, _) => bail!("refused the request (HTTP 403): is its host allowed on that node?"),
        (status, _) => bail!("answered HTTP {status}"),
    }
}

/// One request to a node: its status and body, whatever the status is.
fn exchange(
    node: &Node,
    post: bool,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<(u16, String)> {
    // Also checked here: nodes.toml can be edited by hand.
    check(node)?;
    let deadline = Instant::now() + timeout;
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .context("timed out")
    };
    let (host, port, host_header) = parse_url(&node.url)?;
    let addrs = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("looking up {host}"))?;
    // Each address in turn: `localhost` is often `::1` first.
    let mut connected = Err(anyhow::anyhow!("no address for {host}"));
    for addr in addrs {
        connected = TcpStream::connect_timeout(&addr, left()?).context("connecting");
        if connected.is_ok() {
            break;
        }
    }
    let mut stream = connected?;
    stream.set_write_timeout(Some(left()?))?;
    let mut headers = String::new();
    if !node.token.is_empty() {
        headers += &format!("Authorization: Bearer {}\r\n", node.token);
    }
    if post {
        // A node's own bearer token is also what its page sends as the
        // per-run token, so this is the same proof a browser gives. No
        // `Origin`: there is no page.
        if !node.token.is_empty() {
            headers += &format!("X-Furmi-Token: {}\r\n", node.token);
        }
        headers += &format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
    }
    let method = if post { "POST" } else { "GET" };
    // HTTP/1.0: the answer is never chunked, so the body is whatever follows
    // the headers until the connection closes.
    let request = format!(
        "{method} {path} HTTP/1.0\r\nHost: {host_header}\r\n{headers}Accept: application/json\r\n\r\n{}",
        if post { body } else { "" }
    );
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    let mut chunk = [0; 16 * 1024];
    loop {
        // Per read, the time left overall, so a slow trickle still ends.
        stream.set_read_timeout(Some(left()?))?;
        let n = stream.read(&mut chunk).context("reading the answer")?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
        if raw.len() as u64 > MAX_BODY {
            bail!("its answer is over {} MB", MAX_BODY / (1024 * 1024));
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").context("not an HTTP answer")?;
    let status = head.lines().next().unwrap_or_default();
    let code = status.split(' ').nth(1).and_then(|c| c.parse().ok());
    Ok((
        code.with_context(|| format!("answered `{status}`"))?,
        body.to_string(),
    ))
}

/// What may follow `/api/tasks/<id>` or `/api/schedules/<id>` when forwarded.
const FORWARDED: [&str; 10] = [
    "view", "logs", "edit", "pause", "resume", "delete", "done", "fail", "cancel", "retry",
];

/// Sends a request meant for another node there, if it is one: a task or
/// schedule id of the form `<id>@<node>` (what the merged state calls a row
/// from `<node>`), `GET /api/repos?node=<node>`, or `POST /api/tasks` whose
/// form names a `node`. `None` means it's this machine's own. The answer is
/// the node's, status and all, as JSON, with `id` put back to `<id>@<node>`; a
/// node that can't be reached is a 502.
pub fn proxy(home: &Home, post: bool, url: &str, body: &str) -> Option<(u16, String)> {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let bad = |message: &str| Some((400, json!({ "error": message }).to_string()));
    // The node's own id for what's asked of it, to name in the answer.
    let mut row_id = None;
    let (name, forwarded, body) = match segments.as_slice() {
        ["api", kind @ ("tasks" | "schedules"), id, rest @ ..] if id.contains(['@', '%']) => {
            let decoded = id.replace("%40", "@");
            let (id, name) = decoded.rsplit_once('@')?;
            // One hop only: an id with another `@` in it would be forwarded
            // on by that node, with its tokens.
            if validate_id(id).is_err()
                || rest.len() > 1
                || rest.iter().any(|r| !FORWARDED.contains(r))
            {
                return bad("not a task or schedule of another node");
            }
            row_id = Some((id.to_string(), decoded.clone()));
            let mut forwarded = format!("/api/{kind}/{id}");
            for part in rest {
                forwarded = format!("{forwarded}/{part}");
            }
            if !query.is_empty() {
                forwarded = format!("{forwarded}?{query}");
            }
            (name.to_string(), forwarded, body.to_string())
        }
        ["api", "repos"] => {
            let name = query.split('&').find_map(|p| p.strip_prefix("node="))?;
            (name.to_string(), "/api/repos".to_string(), String::new())
        }
        ["api", "tasks"] if post => {
            let mut form: Value = serde_json::from_str(body).ok()?;
            // No `node`, or this machine: not ours to forward.
            match form.get("node") {
                None | Some(Value::Null) => return None,
                Some(Value::String(n)) if n.is_empty() || n == LOCAL => return None,
                _ => {}
            }
            let name = form["node"].as_str().map(str::to_string);
            let form = form.as_object_mut()?;
            // A form for another node is never run here, whatever it lacks.
            let (Some(name), Some(repo)) = (name, form.remove("repo")) else {
                return bad("a task for another machine needs a node name and a project");
            };
            form.remove("node");
            // The repo field names a remote on another machine, not a path.
            form.insert("remote".into(), repo);
            (
                name,
                "/api/tasks".to_string(),
                Value::Object(form.clone()).to_string(),
            )
        }
        _ => return None,
    };
    let error = |status, message: String| Some((status, json!({ "error": message }).to_string()));
    let node = match load(home) {
        Ok(nodes) => match nodes.into_iter().find(|n| n.name == name) {
            Some(node) => node,
            None => return error(404, format!("no node called `{name}`")),
        },
        Err(e) => return error(500, format!("{e:#}")),
    };
    let timeout = if post { WRITE_TIMEOUT } else { TIMEOUT };
    let answer = exchange(&node, post, &forwarded, &body, timeout);
    if post {
        // Whatever happened, what the page shows next must be asked fresh.
        if let Some(entry) = entries().get_mut(&node.name) {
            entry.last = None;
        }
    }
    let (status, text) = match answer {
        Ok(answer) => answer,
        Err(e) if post => {
            return error(
                502,
                format!(
                    "{name} didn't answer: {e:#} (it may still have applied the change; refresh to check)"
                ),
            );
        }
        Err(e) => return error(502, format!("{name} didn't answer: {e:#}")),
    };
    let Ok(mut answer) = serde_json::from_str::<Value>(&text) else {
        // Not JSON: an older furmi, or a refusal in plain text.
        let hint = match status {
            401 => " (it refused the token)",
            403 => " (it refused the request: changing things there needs its token)",
            _ => "",
        };
        let text: String = text.trim().chars().take(200).collect();
        return error(
            if status < 400 { 502 } else { status },
            format!("{name} answered {status}{hint}: {text}"),
        );
    };
    if let Some((real, full)) = row_id
        && answer["id"].as_str() == Some(real.as_str())
    {
        answer["id"] = json!(full);
    }
    Some((status, answer.to_string()))
}

/// The node name that asks the hub to choose a machine.
pub const AUTO: &str = "auto";

/// What a machine can take on: the remotes it has a clone of, its usable
/// agents, and how loaded it is.
#[derive(Debug, Clone, Default)]
pub struct Capacity {
    pub remotes: Vec<String>,
    pub agents: Vec<String>,
    pub running: usize,
    /// `None` from an older furmi that doesn't say.
    pub max_parallel: Option<usize>,
}

impl Capacity {
    fn from_json(v: &Value) -> Capacity {
        let strings = |key: &str, field: Option<&str>| -> Vec<String> {
            v[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| match field {
                    Some(f) => x[f].as_str(),
                    None => x.as_str(),
                })
                .map(str::to_string)
                .collect()
        };
        Capacity {
            remotes: strings("repos", Some("remote")),
            agents: strings("agents", None),
            running: v["running"].as_u64().unwrap_or(0) as usize,
            max_parallel: v["max_parallel"].as_u64().map(|n| n as usize),
        }
    }

    fn full(&self) -> bool {
        self.max_parallel.is_some_and(|max| self.running >= max)
    }
}

/// The machine to run a task for `remote` (and `agent`, if one is asked for) on:
/// among those that have a clone and the agent, one with a free slot over a full
/// one, then the fewest running, then the first listed (this machine). A queue
/// that's full everywhere still gets a pick, since a task can wait its turn.
fn choose<'a>(
    candidates: &'a [(String, Capacity)],
    remote: &str,
    agent: Option<&str>,
) -> Result<&'a str> {
    let with_clone: Vec<_> = candidates
        .iter()
        .filter(|(_, c)| c.remotes.iter().any(|r| r == remote))
        .collect();
    if with_clone.is_empty() {
        bail!("no machine that is online and can take tasks has a clone of {remote}");
    }
    with_clone
        .into_iter()
        .filter(|(_, c)| agent.is_none_or(|a| c.agents.iter().any(|x| x == a)))
        .min_by_key(|(_, c)| (c.full(), c.running))
        .map(|(name, _)| name.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no machine with a clone of {remote} has the agent `{}`",
                agent.unwrap_or_default()
            )
        })
}

/// Settles a new-task form whose `node` is `auto`: asks every machine that can
/// take tasks what it has, picks one, and returns the form addressed to it
/// (`node` a name, or removed for this machine, whose own form names the
/// project by `remote`) with the chosen machine's name. `None` for any other form.
pub fn resolve_auto(
    home: &Home,
    local: Option<Capacity>,
    body: &str,
) -> Result<Option<(String, String)>> {
    let Ok(Value::Object(mut form)) = serde_json::from_str::<Value>(body) else {
        return Ok(None);
    };
    if form.get("node").and_then(Value::as_str) != Some(AUTO) {
        return Ok(None);
    }
    let remote = form
        .get("repo")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .context("choose the project the agent should work in")?;
    let agent = form
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    let mut candidates: Vec<(String, Capacity)> =
        local.into_iter().map(|c| (LOCAL.to_string(), c)).collect();
    // Only nodes that can be written to; asked at once, since each may be slow.
    let peers: Vec<Node> = load(home)?
        .into_iter()
        .filter(|n| !n.token.is_empty())
        .collect();
    let answers: Vec<Option<Capacity>> = std::thread::scope(|scope| {
        let asked: Vec<_> = peers
            .iter()
            .map(|n| {
                scope.spawn(move || {
                    let text = fetch(n, "/api/repos").ok()?;
                    Some(Capacity::from_json(&serde_json::from_str(&text).ok()?))
                })
            })
            .collect();
        asked.into_iter().map(|t| t.join().ok().flatten()).collect()
    });
    for (node, capacity) in peers.iter().zip(answers) {
        if let Some(capacity) = capacity {
            candidates.push((node.name.clone(), capacity));
        }
    }
    let name = choose(&candidates, &remote, agent.as_deref())?.to_string();
    if name == LOCAL {
        form.remove("node");
        form.remove("repo");
        form.insert("remote".into(), json!(remote));
    } else {
        form.insert("node".into(), json!(name));
    }
    Ok(Some((Value::Object(form).to_string(), name)))
}

type Answer = std::result::Result<Value, String>;

/// What the hub last heard from a node, and whether it is asking right now.
#[derive(Default)]
struct Entry {
    /// url + token the answer was fetched with, so a changed node starts over.
    identity: String,
    last: Option<(Instant, Answer)>,
    asking: bool,
}

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn entries() -> std::sync::MutexGuard<'static, HashMap<String, Entry>> {
    cache().lock().unwrap_or_else(|e| e.into_inner())
}

/// A node's `/api/state`, or why there isn't one. Reused for a moment, and
/// only one request at a time goes out per node: while one is waiting on a slow
/// node, the others get the last answer instead of queueing up behind it.
fn state_of(node: &Node) -> Answer {
    let identity = format!("{}\n{}", node.url, node.token);
    {
        let mut all = entries();
        let entry = all.entry(node.name.clone()).or_default();
        if entry.identity != identity {
            *entry = Entry {
                identity,
                ..Entry::default()
            };
        }
        if let Some((at, answer)) = &entry.last {
            let ttl = if answer.is_ok() {
                CACHE_TTL
            } else {
                FAILURE_TTL
            };
            if at.elapsed() < ttl {
                return answer.clone();
            }
        }
        if entry.asking {
            return match &entry.last {
                Some((_, answer)) => answer.clone(),
                None => Err("still checking".into()),
            };
        }
        entry.asking = true;
    }
    let answer = fetch(node, "/api/state?local=1")
        .and_then(|body| {
            let state: Value = serde_json::from_str(&body).context("not furmi's state")?;
            if !state.is_object() {
                bail!("not furmi's state");
            }
            Ok(state)
        })
        .map_err(|e| format!("{e:#}"));
    if let Some(entry) = entries().get_mut(&node.name) {
        entry.asking = false;
        entry.last = Some((Instant::now(), answer.clone()));
    }
    answer
}

const LISTS: [&str; 4] = ["queue", "running", "archive", "schedules"];

/// `local` (this machine's `/api/state`) with every node's rows added, each row
/// tagged with its `node`, the counts summed, and a `nodes` list saying which
/// answered. With no nodes configured, `local` comes back untouched.
pub fn merge_state(home: &Home, local: String) -> String {
    let nodes = match load(home) {
        Ok(nodes) if nodes.is_empty() => return local,
        Ok(nodes) => nodes,
        Err(e) => {
            // Say so on the page rather than quietly showing one machine.
            let Ok(mut state) = serde_json::from_str::<Value>(&local) else {
                return local;
            };
            state["nodes"] = json!([
                { "name": LOCAL, "url": null, "online": true },
                { "name": "nodes.toml", "url": "", "online": false, "error": format!("{e:#}") },
            ]);
            return state.to_string();
        }
    };
    let Ok(mut state) = serde_json::from_str::<Value>(&local) else {
        return local;
    };
    for list in LISTS {
        tag_rows(&mut state[list], LOCAL, true);
    }
    // In parallel: a node that doesn't answer costs one timeout, not one each.
    let answers: Vec<Answer> = std::thread::scope(|s| {
        let handles: Vec<_> = nodes.iter().map(|n| s.spawn(|| state_of(n))).collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("crashed".into())))
            .collect()
    });
    let mut summary = vec![json!({ "name": LOCAL, "url": null, "online": true, "writable": true })];
    for (node, answer) in nodes.iter().zip(answers) {
        match answer {
            Ok(mut theirs) => {
                for list in LISTS {
                    tag_rows(&mut theirs[list], &node.name, !node.token.is_empty());
                    if let (Some(rows), Some(mine)) =
                        (theirs[list].as_array().cloned(), state[list].as_array_mut())
                    {
                        mine.extend(rows);
                    }
                }
                add_counts(&mut state["counts"], &theirs["counts"]);
                summary.push(json!({ "name": node.name, "url": node.url, "online": true, "writable": !node.token.is_empty() }));
            }
            Err(error) => summary.push(
                json!({ "name": node.name, "url": node.url, "online": false, "error": error }),
            ),
        }
    }
    sort_merged(&mut state);
    state["nodes"] = Value::Array(summary);
    state.to_string()
}

/// Tags each row with its node and whether it can be changed. Another node's rows also get `@<node>` on
/// their id: ids are only unique on one machine, and this is the id the page
/// sends back, which [`proxy`] reads to know where a request goes.
fn tag_rows(rows: &mut Value, node: &str, writable: bool) {
    for row in rows.as_array_mut().into_iter().flatten() {
        if let Some(row) = row.as_object_mut() {
            row.insert("node".into(), json!(node));
            // Whether this page can change it: a node added without a token
            // can only be read.
            row.insert("writable".into(), json!(writable));
            if node != LOCAL
                && let Some(id) = row.get("id").and_then(Value::as_str)
            {
                let id = format!("{id}@{node}");
                row.insert("id".into(), json!(id));
            }
        }
    }
}

/// Puts each merged list back in the order a single machine serves it: the
/// queue and running soonest first, the archive newest first. Rows with no
/// time ("as soon as possible") come before the rest.
fn sort_merged(state: &mut Value) {
    let when = |row: &Value| {
        row["when_at"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    };
    for (list, newest_first) in [("queue", false), ("running", false), ("archive", true)] {
        if let Some(rows) = state[list].as_array_mut() {
            // Stable, so ties keep this machine first.
            rows.sort_by(|a, b| {
                let order = when(a).cmp(&when(b));
                if newest_first { order.reverse() } else { order }
            });
        }
    }
}

/// Adds the task counts of `theirs` to `ours`. `invalid` stays this machine's:
/// those are files the page lists, and only this machine's are listed.
fn add_counts(ours: &mut Value, theirs: &Value) {
    let (Some(ours), Some(theirs)) = (ours.as_object_mut(), theirs.as_object()) else {
        return;
    };
    for (key, n) in theirs {
        if key == "invalid" {
            continue;
        }
        if let (Some(a), Some(b)) = (ours.get(key).and_then(Value::as_u64), n.as_u64()) {
            ours.insert(key.clone(), json!(a + b));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        (dir, home)
    }

    #[test]
    fn urls_are_plain_http_hosts() {
        assert_eq!(
            parse_url("http://box.ts.net:7433").unwrap(),
            ("box.ts.net".into(), 7433, "box.ts.net:7433".into())
        );
        assert_eq!(parse_url("http://box").unwrap().1, 80);
        assert_eq!(parse_url("http://[::1]:7433").unwrap().0, "::1");
        for bad in [
            "https://box:7433",
            "box:7433",
            "http://",
            "http://box/path",
            "http://user:pw@box",
            "http://box:abc",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn nodes_round_trip_through_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, home) = home();
        assert!(load(&home).unwrap().is_empty());
        let node = Node {
            name: "box".into(),
            url: "http://box:7433".into(),
            token: "secret".into(),
        };
        save(&home, vec![node.clone()]).unwrap();
        assert_eq!(load(&home).unwrap(), vec![node]);
        let mode = std::fs::metadata(path(&home)).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn names_are_ids_and_not_the_local_one() {
        let node = |name: &str| Node {
            name: name.into(),
            url: "http://box:7433".into(),
            token: String::new(),
        };
        assert!(check(&node("box-2")).is_ok());
        assert!(check(&node("Box")).is_err());
        assert!(check(&node(LOCAL)).is_err());
    }

    #[test]
    fn without_nodes_the_state_is_untouched() {
        let (_dir, home) = home();
        let local = r#"{"queue":[]}"#.to_string();
        assert_eq!(merge_state(&home, local.clone()), local);
    }

    /// A node that answers `body` to every request, once per connection.
    fn cap(remotes: &[&str], agents: &[&str], running: usize, max: Option<usize>) -> Capacity {
        Capacity {
            remotes: remotes.iter().map(|s| s.to_string()).collect(),
            agents: agents.iter().map(|s| s.to_string()).collect(),
            running,
            max_parallel: max,
        }
    }

    #[test]
    fn auto_picks_the_least_busy_machine_with_the_repo_and_agent() {
        let app = "github.com/me/app";
        let all = vec![
            ("local".to_string(), cap(&[app], &["claude"], 2, Some(3))),
            (
                "box".to_string(),
                cap(&[app], &["claude", "codex"], 1, Some(3)),
            ),
            (
                "nas".to_string(),
                cap(&["github.com/me/other"], &["claude"], 0, Some(3)),
            ),
        ];
        assert_eq!(choose(&all, app, None).unwrap(), "box");
        // Only box has codex.
        assert_eq!(choose(&all, app, Some("codex")).unwrap(), "box");
        let err = choose(&all, app, Some("pi")).unwrap_err().to_string();
        assert!(err.contains("`pi`"), "{err}");
        let err = choose(&all, "github.com/me/none", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no machine"), "{err}");
    }

    #[test]
    fn auto_prefers_a_free_slot_then_this_machine_on_a_tie() {
        let app = "github.com/me/app";
        // box is idle but full; local has a slot despite running more.
        let all = vec![
            ("local".to_string(), cap(&[app], &[], 1, Some(2))),
            ("box".to_string(), cap(&[app], &[], 0, Some(0))),
        ];
        assert_eq!(choose(&all, app, None).unwrap(), "local");
        // Full everywhere: still placed, on the least loaded.
        let full = vec![
            ("local".to_string(), cap(&[app], &[], 3, Some(3))),
            ("box".to_string(), cap(&[app], &[], 2, Some(2))),
        ];
        assert_eq!(choose(&full, app, None).unwrap(), "box");
        // A tie goes to the first listed.
        let tie = vec![
            ("local".to_string(), cap(&[app], &[], 1, None)),
            ("box".to_string(), cap(&[app], &[], 1, None)),
        ];
        assert_eq!(choose(&tie, app, None).unwrap(), "local");
    }

    #[test]
    fn auto_forms_are_rewritten_for_the_chosen_machine() {
        let (_dir, home) = home();
        let app = "github.com/me/app";
        let form = format!(r#"{{"prompt":"p","repo":"{app}","node":"auto"}}"#);
        // Not auto: left alone.
        assert!(
            resolve_auto(&home, None, r#"{"node":"box"}"#)
                .unwrap()
                .is_none()
        );
        // Only this machine has it: the form becomes a local one, by remote.
        let local = cap(&[app], &[], 0, Some(3));
        let (body, name) = resolve_auto(&home, Some(local), &form).unwrap().unwrap();
        assert_eq!(name, "local");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["remote"], app);
        assert!(body.get("node").is_none() && body.get("repo").is_none());
        // Nobody has it.
        let err = resolve_auto(&home, None, &form).unwrap_err().to_string();
        assert!(err.contains("no machine"), "{err}");
    }

    #[test]
    fn auto_asks_the_other_machines_what_they_have() {
        let (_dir, home) = home();
        let url = serve(
            r#"{"repos":[{"remote":"github.com/me/app","path":"~/app"}],"agents":["claude"],"agent":"claude","running":0,"max_parallel":3}"#,
            "200 OK",
        );
        save(
            &home,
            vec![
                Node {
                    name: "box".into(),
                    url: url.clone(),
                    token: "t".into(),
                },
                // Read-only: never picked.
                Node {
                    name: "ro".into(),
                    url,
                    token: String::new(),
                },
            ],
        )
        .unwrap();
        let busy = cap(&["github.com/me/app"], &["claude"], 3, Some(3));
        let form = r#"{"prompt":"p","repo":"github.com/me/app","node":"auto"}"#;
        let (body, name) = resolve_auto(&home, Some(busy), form).unwrap().unwrap();
        assert_eq!(name, "box");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["node"], "box");
        assert_eq!(body["repo"], "github.com/me/app");
    }

    fn serve(body: &'static str, status: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                // The whole request, as a real server reads it, so closing
                // doesn't reset the connection under the answer.
                let mut seen = Vec::new();
                let mut buf = [0; 512];
                while !seen.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                        _ => break,
                    }
                }
                let _ = write!(
                    stream,
                    "HTTP/1.0 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn a_nodes_rows_join_ours_tagged_and_a_dead_node_is_offline() {
        let (_dir, home) = home();
        let up = serve(
            r#"{"counts":{"pending":2,"done":1,"invalid":5},"queue":[{"id":"b"}],
                "running":[],"archive":[],"schedules":[]}"#,
            "200 OK",
        );
        let refused = serve("nope", "401 Unauthorized");
        save(
            &home,
            vec![
                Node {
                    name: "up".into(),
                    url: up,
                    token: "t".into(),
                },
                Node {
                    name: "refused".into(),
                    url: refused,
                    token: "t".into(),
                },
                // Nothing listens on port 9 of loopback.
                Node {
                    name: "gone".into(),
                    url: "http://127.0.0.1:9".into(),
                    token: String::new(),
                },
            ],
        )
        .unwrap();
        let local = r#"{"counts":{"pending":1,"done":0,"invalid":1},"queue":[{"id":"a"}],
            "running":[],"archive":[],"schedules":[]}"#;

        let merged: Value = serde_json::from_str(&merge_state(&home, local.into())).unwrap();

        assert_eq!(merged["queue"][0]["node"], "local");
        assert_eq!(merged["queue"][1]["id"], "b@up");
        assert_eq!(merged["queue"][1]["node"], "up");
        assert_eq!(merged["counts"]["pending"], 3);
        assert_eq!(merged["counts"]["done"], 1);
        assert_eq!(merged["counts"]["invalid"], 1);
        let online: Vec<_> = merged["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| (n["name"].as_str().unwrap(), n["online"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            online,
            [
                ("local", true),
                ("up", true),
                ("refused", false),
                ("gone", false)
            ]
        );
        assert!(
            merged["nodes"][2]["error"]
                .as_str()
                .unwrap()
                .contains("401")
        );
    }

    fn node(name: &str, url: String) -> Node {
        Node {
            name: name.into(),
            url,
            token: String::new(),
        }
    }

    #[test]
    fn a_reply_that_is_not_furmis_state_is_an_error_not_a_panic() {
        let (_dir, home) = home();
        let array = serve("[]", "200 OK");
        let rows = serve(r#"{"queue":[1,"x"],"counts":5}"#, "200 OK");
        save(&home, vec![node("array", array), node("rows", rows)]).unwrap();
        let local =
            r#"{"counts":{"pending":0},"queue":[],"running":[],"archive":[],"schedules":[]}"#;

        let merged: Value = serde_json::from_str(&merge_state(&home, local.into())).unwrap();

        assert_eq!(merged["nodes"][1]["online"], false);
        assert_eq!(merged["nodes"][2]["online"], true);
        assert_eq!(merged["counts"]["pending"], 0);
    }

    #[test]
    fn a_node_that_trickles_is_cut_off_at_the_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // A byte every half second, for far longer than the deadline.
            for _ in 0..40 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        let started = Instant::now();
        let err = fetch(&node("slow", url), "/api/state").unwrap_err();
        assert!(
            started.elapsed() < TIMEOUT + Duration::from_secs(1),
            "{err:#}"
        );
    }

    #[test]
    fn a_changed_token_does_not_get_the_old_answer() {
        let (_dir, home) = home();
        let url = serve(r#"{"queue":[]}"#, "200 OK");
        let mut n = node("cache-test", url);
        assert!(state_of(&n).is_ok());
        n.url = "http://127.0.0.1:9".into();
        assert!(state_of(&n).is_err());
        drop(home);
    }

    #[test]
    fn header_injection_and_odd_urls_are_refused() {
        let mut n = node("box", "http://box:7433".into());
        n.token = "abc\r\nX-Evil: 1".into();
        assert!(check(&n).is_err());
        n.token = String::new();
        n.url = "http://box:7433\r\nX: 1".into();
        assert!(check(&n).is_err());
    }

    #[test]
    fn only_loopback_and_tailnet_hosts_are_private() {
        for yes in [
            "127.0.0.1",
            "::1",
            "localhost",
            "box.tailnet.ts.net",
            "100.64.0.10",
        ] {
            assert!(private_network(yes), "{yes}");
        }
        for no in ["203.0.113.5", "box", "100.128.0.1", "example.com"] {
            assert!(!private_network(no), "{no}");
        }
    }

    #[test]
    fn merged_lists_keep_each_machines_order() {
        let mut state = json!({
            "queue": [{"id": "late", "when_at": "2026-10-31T10:00:00+00:00"},
                      {"id": "asap", "when_at": null},
                      {"id": "early", "when_at": "2026-10-31T09:00:00+00:00"}],
            "archive": [{"id": "old", "when_at": "2026-10-01T09:00:00+00:00"},
                        {"id": "new", "when_at": "2026-10-30T09:00:00+00:00"}],
        });
        sort_merged(&mut state);
        let ids = |list: &str| -> Vec<String> {
            state[list]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(ids("queue"), ["asap", "early", "late"]);
        assert_eq!(ids("archive"), ["new", "old"]);
    }

    #[test]
    fn a_nodes_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, home) = home();
        save(&home, vec![node("box", "http://box:7433".into())]).unwrap();
        std::fs::set_permissions(path(&home), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&home).is_err());
    }

    /// A node that records the request it gets and answers `status` + `body`.
    fn capture(
        status: &'static str,
        body: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut buf = [0; 1024];
            // Headers, then as much body as Content-Length says.
            loop {
                let n = stream.read(&mut buf).unwrap();
                seen.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&seen).to_string();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .map_or(0, |n| n.parse().unwrap());
                    if rest.len() >= want || n == 0 {
                        break;
                    }
                }
            }
            tx.send(String::from_utf8_lossy(&seen).to_string()).unwrap();
            let _ = write!(stream, "HTTP/1.0 {status}\r\n\r\n{body}");
        });
        (url, rx)
    }

    fn with_node(home: &Home, name: &str, url: String) {
        save(
            home,
            vec![Node {
                name: name.into(),
                url,
                token: "tok".into(),
            }],
        )
        .unwrap();
    }

    #[test]
    fn an_action_on_another_nodes_row_goes_to_that_node_with_its_own_id() {
        let (_dir, home) = home();
        let (url, request) = capture("400 Bad Request", r#"{"error":"nope"}"#);
        with_node(&home, "box", url);

        let (status, body) = proxy(
            &home,
            true,
            "/api/tasks/fix-ci%40box/fail",
            r#"{"reason":"x"}"#,
        )
        .unwrap();

        assert_eq!((status, body.as_str()), (400, r#"{"error":"nope"}"#));
        let sent = request.recv().unwrap();
        assert!(
            sent.starts_with("POST /api/tasks/fix-ci/fail HTTP/1.0"),
            "{sent}"
        );
        assert!(sent.contains("Authorization: Bearer tok"));
        assert!(sent.contains("X-Furmi-Token: tok"));
        assert!(sent.ends_with(r#"{"reason":"x"}"#));
    }

    #[test]
    fn a_task_for_another_node_is_sent_by_remote_not_path() {
        let (_dir, home) = home();
        let (url, request) = capture("200 OK", r#"{"ok":true,"id":"t"}"#);
        with_node(&home, "box", url);
        let form = r#"{"prompt":"p","repo":"github.com/me/app","node":"box","agent":"claude"}"#;

        let (status, _) = proxy(&home, true, "/api/tasks", form).unwrap();

        assert_eq!(status, 200);
        let sent = request.recv().unwrap();
        let body: Value = serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["remote"], "github.com/me/app");
        assert_eq!(body["prompt"], "p");
        assert!(body.get("repo").is_none() && body.get("node").is_none());
    }

    #[test]
    fn this_machines_own_requests_are_not_proxied() {
        let (_dir, home) = home();
        with_node(&home, "box", "http://127.0.0.1:9".into());
        assert!(proxy(&home, false, "/api/tasks/fix-ci/view", "").is_none());
        assert!(
            proxy(
                &home,
                true,
                "/api/tasks",
                r#"{"prompt":"p","repo":"/x","node":"local"}"#
            )
            .is_none()
        );
        assert!(proxy(&home, true, "/api/tasks", r#"{"prompt":"p","repo":"/x"}"#).is_none());
        assert!(proxy(&home, false, "/api/repos", "").is_none());
        // Unknown and unreachable nodes are errors, never silently local.
        assert_eq!(
            proxy(&home, false, "/api/tasks/a%40nope/view", "")
                .unwrap()
                .0,
            404
        );
        assert_eq!(
            proxy(&home, false, "/api/repos?node=box", "").unwrap().0,
            502
        );
    }

    #[test]
    fn what_a_node_says_about_a_row_keeps_the_rows_full_id() {
        let (_dir, home) = home();
        let (url, _request) = capture("200 OK", r#"{"id":"fix-ci","prompt":"p"}"#);
        with_node(&home, "box", url);

        let (_, body) = proxy(&home, false, "/api/tasks/fix-ci%40box", "").unwrap();

        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["id"], "fix-ci@box");
        assert_eq!(body["prompt"], "p");
    }

    #[test]
    fn only_one_hop_and_only_known_paths_are_forwarded() {
        let (_dir, home) = home();
        with_node(&home, "b", "http://127.0.0.1:9".into());
        for url in [
            "/api/tasks/x%40a%40b/cancel",
            "/api/tasks/%40b",
            "/api/tasks/x%40b/..",
            "/api/tasks/x%40b/view/extra",
            "/api/tasks/X%40b/view",
        ] {
            assert_eq!(proxy(&home, true, url, "{}").unwrap().0, 400, "{url}");
        }
    }

    #[test]
    fn a_form_naming_a_node_never_falls_back_to_this_machine() {
        let (_dir, home) = home();
        with_node(&home, "box", "http://127.0.0.1:9".into());
        for form in [
            r#"{"prompt":"p","node":"box","remote":"github.com/me/app"}"#,
            r#"{"prompt":"p","node":5,"repo":"x"}"#,
        ] {
            assert_eq!(
                proxy(&home, true, "/api/tasks", form).unwrap().0,
                400,
                "{form}"
            );
        }
    }

    #[test]
    fn a_plain_text_refusal_comes_back_as_a_readable_json_error() {
        let (_dir, home) = home();
        let (url, _request) = capture("403 Forbidden", "forbidden: missing or wrong token");
        with_node(&home, "box", url);

        let (status, body) = proxy(&home, true, "/api/tasks/a%40box/cancel", "{}").unwrap();

        assert_eq!(status, 403);
        let error = serde_json::from_str::<Value>(&body).unwrap()["error"].to_string();
        assert!(error.contains("needs its token"), "{error}");
    }

    #[test]
    fn rows_say_whether_their_node_can_be_changed() {
        let (_dir, home) = home();
        let up = serve(
            r#"{"queue":[{"id":"b"}],"running":[],"archive":[],"schedules":[]}"#,
            "200 OK",
        );
        save(&home, vec![node("readonly", up)]).unwrap();
        let local =
            r#"{"counts":{},"queue":[{"id":"a"}],"running":[],"archive":[],"schedules":[]}"#;

        let merged: Value = serde_json::from_str(&merge_state(&home, local.into())).unwrap();

        assert_eq!(merged["queue"][0]["writable"], true);
        assert_eq!(merged["queue"][1]["writable"], false);
        assert_eq!(merged["nodes"][1]["writable"], false);
    }
}
