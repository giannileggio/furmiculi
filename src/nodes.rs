//! Other machines running furmi, whose queues the dashboard shows next to this
//! one's.
//!
//! Each node is a self-contained furmi with its own queue. The hub (the machine
//! you open) asks every node for its `/api/state`, tags the rows with the
//! node's name and merges them. Nothing is shared or synced: a node that is
//! down is shown as offline and its rows drop out, and everything else keeps
//! working.
//!
//! The hub reaches a node over SSH, the way herdr reaches its machines: it runs
//! `furmi node call <METHOD> <path>` there, which answers the dashboard's own
//! route in-process. So a node needs no open port, no running dashboard and no
//! token, and authentication, host keys and encryption are all OpenSSH's. The
//! list lives in `nodes.toml` beside `config.toml` and holds only a name and an
//! SSH target for each.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    /// What `ssh` is given: a host from `~/.ssh/config`, `user@host`, or
    /// `ssh://user@host:port`.
    pub ssh: String,
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

/// The nodes for `add` and `rm`: a file that won't parse (one from before ssh
/// nodes) counts as empty, so these can still replace it.
fn load_for_edit(home: &Home) -> Vec<Node> {
    load(home).unwrap_or_else(|e| {
        eprintln!("warning: dropping the nodes in the old nodes.toml: {e:#}");
        Vec::new()
    })
}

pub fn load(home: &Home) -> Result<Vec<Node>> {
    let path = path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: NodesFile = toml::from_str(&text).with_context(|| {
        format!(
            "parsing {} (nodes used to be a url and a token; they are an SSH target now, so \
             `furmi node add <name> <ssh-target>` again, or `furmi node rm` to drop them all)",
            path.display()
        )
    })?;
    Ok(file.nodes)
}

fn save(home: &Home, nodes: Vec<Node>) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = path(home);
    let body = toml::to_string(&NodesFile { nodes })?;
    let text = format!("# Written by `furmi node`.\n{body}");
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
    /// SSH-TARGET is anything `ssh` takes: a host from ~/.ssh/config,
    /// `user@host`, or `ssh://user@host:2222`. Check that `ssh <target>` works
    /// first; furmi uses your keys, agent and known hosts, and needs furmi
    /// installed on the other machine.
    Add(AddArgs),
    /// List the nodes and whether they answer.
    Ls,
    /// Stop showing a node.
    Rm { name: String },
    /// Answer one dashboard request on this machine and print the result. What
    /// the hub runs on a node over SSH; not meant to be typed.
    #[command(hide = true)]
    Call {
        /// GET or POST.
        method: String,
        /// The path and query, e.g. `/api/state?local=1`. A POST's body is
        /// read from standard input.
        path: String,
    },
}

#[derive(Args)]
pub struct AddArgs {
    /// A short name for the node, shown beside its tasks (a-z, 0-9, `-`).
    name: String,
    /// e.g. `box`, `me@box.tailnet.ts.net` or `ssh://me@box:2222`.
    ssh_target: String,
}

pub fn run(home: &Home, cmd: NodeCmd) -> Result<()> {
    match cmd {
        NodeCmd::Add(args) => add(home, args),
        NodeCmd::Ls => ls(home),
        NodeCmd::Call { method, path } => call(home, &method, &path),
        NodeCmd::Rm { name } => {
            let _lock = home.lock()?;
            let mut nodes = load_for_edit(home);
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

/// How long `node add` waits: the person may be typing a passphrase or
/// accepting a host key.
const ADD_TIMEOUT: Duration = Duration::from_secs(120);

fn add(home: &Home, args: AddArgs) -> Result<()> {
    let node = Node {
        name: args.name,
        ssh: args.ssh_target.trim().to_string(),
    };
    check(&node)?;
    // Like herdr: nothing is saved for a machine that can't be reached, so a
    // typo doesn't leave a node that is always offline. The connection made
    // here stays open for a while, so the dashboard's calls reuse it.
    match exchange(
        home,
        &node,
        false,
        "/api/state?local=1",
        "",
        ADD_TIMEOUT,
        true,
    ) {
        Ok((200, _)) => {}
        Ok((status, _)) => bail!("{} answered HTTP {status}", node.ssh),
        Err(e) => bail!("can't reach {} over ssh: {e:#}", node.ssh),
    }
    {
        let _lock = home.lock()?;
        let mut nodes = load_for_edit(home);
        // Same name: replaced, so adding again is how a target is changed.
        nodes.retain(|n| n.name != node.name);
        nodes.push(node.clone());
        save(home, nodes)?;
    }
    println!("added {} ({})", node.name, node.ssh);
    Ok(())
}

fn ls(home: &Home) -> Result<()> {
    let nodes = load(home)?;
    if nodes.is_empty() {
        println!("no nodes; add one with `furmi node add <name> <ssh-target>`");
        return Ok(());
    }
    let statuses: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = nodes
            .iter()
            .map(|n| {
                s.spawn(|| match fetch(home, n, "/api/state?local=1") {
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
        println!("{:<16} {:<40} {status}", node.name, node.ssh);
    }
    Ok(())
}

fn check(node: &Node) -> Result<()> {
    validate_id(&node.name).map_err(anyhow::Error::msg)?;
    if node.name == LOCAL || node.name == AUTO {
        bail!("`{}` is a reserved name; pick another", node.name);
    }
    // Also checked on every call: nodes.toml can be edited by hand. The target
    // is one argument to ssh, never a shell line, but a leading `-` would be
    // read as an option.
    if node.ssh.is_empty()
        || node.ssh.starts_with('-')
        || node
            .ssh
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        bail!(
            "`{}` isn't an ssh target: use a host from ~/.ssh/config, user@host or \
             ssh://user@host:port",
            node.ssh
        );
    }
    Ok(())
}

/// One GET to a node, returning the body of a 200.
fn fetch(home: &Home, node: &Node, path: &str) -> Result<String> {
    match exchange(home, node, false, path, "", TIMEOUT, false)? {
        (200, body) => Ok(body),
        (status, body) => {
            let said = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string));
            match said {
                Some(said) => bail!("answered HTTP {status}: {said}"),
                None => bail!("answered HTTP {status}"),
            }
        }
    }
}

/// Starts every remote command: the usual install places aren't on the PATH
/// of a non-interactive ssh session.
const REMOTE_PATH: &str = r#"PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH""#;

/// Marks where the node's answer starts, so anything a login script prints
/// first is ignored.
const ANSWER_MARK: &str = "\u{1}furmi ";

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The directory ssh keeps its shared connections in (0700), so the dashboard
/// polling a node every couple of seconds doesn't log in each time.
fn control_dir(home: &Home) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = home.root().join("ssh");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// A first login gets longer than a request on a connection already open.
const COLD_TIMEOUT: Duration = Duration::from_secs(8);

/// Longest folder ssh can still put its control socket in: the path is limited
/// to about 107 bytes, and ssh adds 17 while setting one up.
const MAX_CONTROL_DIR: usize = 45;

/// ssh's options for sharing one connection per node, or none when the folder
/// can't be named safely: ssh reads `%` and quotes in the value, and the
/// socket path has a length limit.
fn control_options(dir: &Path) -> Vec<String> {
    let dir = dir.to_string_lossy();
    if dir.len() > MAX_CONTROL_DIR || dir.contains(['"', '\\', '\n']) {
        return vec!["-o".into(), "ControlMaster=no".into()];
    }
    let escaped = dir.replace('%', "%%");
    vec![
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPersist=600".into(),
        "-o".into(),
        format!("ControlPath=\"{escaped}/%C\""),
    ]
}

/// One request to a node: its status and body, whatever the status is. With
/// `interactive`, ssh may ask the person (a passphrase, an unknown host key);
/// otherwise it must not, since nobody is there to answer.
fn exchange(
    home: &Home,
    node: &Node,
    post: bool,
    path: &str,
    body: &str,
    timeout: Duration,
    interactive: bool,
) -> Result<(u16, String)> {
    check(node)?;
    #[cfg(test)]
    if let Some(fake) = tests::fake(&node.ssh) {
        return fake(post, path, body);
    }
    let control = control_dir(home)?;
    // A first login (no shared connection yet) is several round trips more
    // than a reused one, and killing it half way keeps no connection to reuse.
    let cold = std::fs::read_dir(&control).is_ok_and(|mut d| d.next().is_none());
    let timeout = if cold {
        timeout.max(COLD_TIMEOUT)
    } else {
        timeout
    };
    let deadline = Instant::now() + timeout;
    let program = std::env::var("FURMI_SSH").unwrap_or_else(|_| "ssh".into());
    let mut ssh = Command::new(program);
    ssh.arg("-T")
        .args(["-o", "ClearAllForwardings=yes", "-o", "LogLevel=ERROR"])
        // A host that opens tmux or the like for you would refuse a command.
        .args(["-o", "RemoteCommand=none"])
        .args([
            "-o",
            &format!("ConnectTimeout={}", timeout.as_secs().max(1)),
        ])
        .args(control_options(&control));
    if !interactive {
        ssh.args(["-o", "BatchMode=yes"]);
    }
    let remote = format!(
        "{REMOTE_PATH} furmi node call {} {}",
        if post { "POST" } else { "GET" },
        shell_quote(path)
    );
    let mut child = ssh
        .arg("--")
        .arg(&node.ssh)
        .arg(remote)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh (is it installed?)")?;
    // The request body goes in on stdin, off this thread so a big one can't
    // block against a node that isn't reading.
    let mut stdin = child.stdin.take().context("ssh has no stdin")?;
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = stdin.write_all(body.as_bytes());
    });
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(MAX_BODY + 1).read_to_end(&mut out);
            }
            let _ = tx.send(out);
        });
        rx
    };
    let out = read(child.stdout.take().map(|p| Box::new(p) as _));
    let err = read(child.stderr.take().map(|p| Box::new(p) as _));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out");
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    // The background connection ssh leaves behind has these pipes closed, but
    // don't wait on that: a moment, then take what has arrived.
    let grace = Instant::now() + Duration::from_secs(1);
    let left = || grace.saturating_duration_since(Instant::now());
    let out = out.recv_timeout(left()).unwrap_or_default();
    let err = err.recv_timeout(left()).unwrap_or_default();
    if out.len() as u64 > MAX_BODY {
        bail!("its answer is over {} MB", MAX_BODY / (1024 * 1024));
    }
    let out = String::from_utf8_lossy(&out);
    let err = String::from_utf8_lossy(&err);
    let err = err.trim();
    if let Some((_, rest)) = out.split_once(ANSWER_MARK)
        && let Some((code, body)) = rest.split_once('\n')
        && let Ok(code) = code.trim().parse()
    {
        return Ok((code, body.to_string()));
    }
    // No answer: say why in the terms of what to do about it.
    let hint = if status.code() == Some(255) {
        String::new()
    } else if status.code() == Some(127) || err.contains("not found") {
        " (furmi isn't installed there, or isn't in ~/.cargo/bin or ~/.local/bin)".into()
    } else if err.contains("unrecognized subcommand") || err.contains("unexpected argument") {
        " (its furmi is too old to be a node: update it with `furmi update`)".into()
    } else {
        format!(
            " (exit {})",
            status.code().map_or("signal".into(), |c| c.to_string())
        )
    };
    let err: String = err
        .lines()
        .last()
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect();
    bail!("{}{hint}", if err.is_empty() { "no answer" } else { &err })
}

/// `furmi node call`: answers one dashboard request here, in-process, and
/// prints it after [`ANSWER_MARK`]. Only the dashboard's `/api` routes, and
/// never forwarded on to another node.
fn call(home: &Home, method: &str, url: &str) -> Result<()> {
    let mut body = String::new();
    if method == "POST" {
        std::io::stdin()
            .take(MAX_BODY)
            .read_to_string(&mut body)
            .context("reading the request body")?;
    }
    let (status, answer) = crate::web::call(home, method, url, &body)?;
    println!("{ANSWER_MARK}{status}\n{answer}");
    Ok(())
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
            // on by that node, as if it were a hub.
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
        ["api", kind @ ("tasks" | "schedules")] if post => {
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
            // With no project (`scratch`) there's nothing to look up there.
            let scratch = form.get("scratch") == Some(&Value::Bool(true));
            let repo = form.remove("repo");
            let (Some(name), true) = (name, scratch || repo.is_some()) else {
                return bad("a form for another machine needs a node name and a project");
            };
            form.remove("node");
            // The repo field names a remote on another machine, not a path.
            if let Some(repo) = repo.filter(|_| !scratch) {
                form.insert("remote".into(), repo);
            }
            (
                name,
                format!("/api/{kind}"),
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
    let answer = exchange(home, &node, post, &forwarded, &body, timeout, false);
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
        // Not JSON: something other than furmi's route answered.
        let text: String = text.trim().chars().take(200).collect();
        return error(
            if status < 400 { 502 } else { status },
            format!("{name} answered {status}: {text}"),
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
    /// Waiting tasks: they will take slots too, so they count as load.
    pub queued: usize,
    /// Running tasks per remote.
    pub running_in: HashMap<String, usize>,
    pub runner_alive: bool,
    /// `None` from a furmi that doesn't say (too old to weigh).
    pub max_parallel: Option<usize>,
    pub max_parallel_per_repo: Option<usize>,
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
        let count = |key: &str| v[key].as_u64().map(|n| n as usize);
        Capacity {
            remotes: strings("repos", Some("remote")),
            agents: strings("agents", None),
            running: count("running").unwrap_or(0),
            queued: count("queued").unwrap_or(0),
            running_in: v["running_in"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, n)| Some((k.clone(), n.as_u64()? as usize)))
                .collect(),
            runner_alive: v["runner_alive"].as_bool().unwrap_or(false),
            max_parallel: count("max_parallel"),
            max_parallel_per_repo: count("max_parallel_per_repo"),
        }
    }

    /// Can start work at all: its runner is up, and it said what its limits are.
    fn usable(&self) -> bool {
        self.runner_alive && self.max_parallel.is_some() && self.max_parallel_per_repo.is_some()
    }

    /// No slot to start a task for `remote` in right now.
    fn full(&self, remote: &str) -> bool {
        self.max_parallel.is_some_and(|max| self.running >= max)
            || self
                .max_parallel_per_repo
                .is_some_and(|max| self.running_in.get(remote).copied().unwrap_or(0) >= max)
    }
}

/// The machine to run a task for `remote` (and `agent`, if one is asked for) on:
/// among those whose runner is up and that have a clone and a usable agent (the
/// one asked for), one with a free slot for that repo over one without, then the
/// least running and waiting, then the first listed (this machine). A queue
/// that's full everywhere still gets a pick, since a task can wait its turn.
fn choose<'a>(
    candidates: &'a [(String, Capacity)],
    remote: Option<&str>,
    agent: Option<&str>,
) -> Result<&'a str> {
    // No remote: a scratch task, which needs no clone, only a free machine.
    let has = |c: &Capacity| remote.is_none_or(|r| c.remotes.iter().any(|x| x == r));
    let with_clone: Vec<_> = candidates
        .iter()
        .filter(|(_, c)| c.usable() && has(c))
        .collect();
    if with_clone.is_empty() {
        match remote {
            Some(remote) => bail!(
                "no machine that is online, can take new work and has its runner started has a clone of {remote}"
            ),
            None => bail!("no machine is online, can take new work and has its runner started"),
        }
    }
    let remote = remote.unwrap_or_default();
    let with = if remote.is_empty() {
        String::new()
    } else {
        format!(" with a clone of {remote}")
    };
    with_clone
        .into_iter()
        .filter(|(_, c)| match agent {
            Some(a) => c.agents.iter().any(|x| x == a),
            None => !c.agents.is_empty(),
        })
        .min_by_key(|(_, c)| (c.full(remote), c.running + c.queued))
        .map(|(name, _)| name.as_str())
        .ok_or_else(|| match agent {
            Some(a) => anyhow::anyhow!("no machine{with} has the agent `{a}`"),
            None => anyhow::anyhow!("no machine{with} has an agent installed"),
        })
}

/// Whether the last try at this node, a moment ago, found it not answering.
fn known_down(name: &str) -> bool {
    entries()
        .get(name)
        .is_some_and(|e| matches!(&e.last, Some((at, Err(_))) if at.elapsed() < FAILURE_TTL))
}

/// Settles a new-task form whose `node` is `auto`: asks every machine that can
/// take tasks what it has, picks one, and returns the form addressed to it
/// (`node` a name, or removed for this machine, whose own form names the
/// project by `remote`) with the chosen machine's name. `None` for any other form.
pub fn resolve_auto(
    home: &Home,
    local: impl FnOnce() -> Result<Capacity>,
    body: &str,
) -> Result<Option<(String, String)>> {
    let Ok(Value::Object(mut form)) = serde_json::from_str::<Value>(body) else {
        return Ok(None);
    };
    if form.get("node").and_then(Value::as_str) != Some(AUTO) {
        return Ok(None);
    }
    // No project (`scratch`): any machine will do.
    let scratch = form.get("scratch") == Some(&Value::Bool(true));
    let remote = if scratch {
        None
    } else {
        Some(
            form.get("repo")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_string)
                .context("choose the project the agent should work in")?,
        )
    };
    let agent = form
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    let mut candidates = vec![(LOCAL.to_string(), local()?)];
    // Only nodes that weren't just found down; asked at
    // once, since each may be slow.
    let peers: Vec<Node> = load(home)?
        .into_iter()
        .filter(|n| !known_down(&n.name))
        .collect();
    let answers: Vec<Option<Capacity>> = std::thread::scope(|scope| {
        let asked: Vec<_> = peers
            .iter()
            .map(|n| {
                scope.spawn(move || {
                    let text = fetch(home, n, "/api/repos").ok()?;
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
    let name = choose(&candidates, remote.as_deref(), agent.as_deref())?.to_string();
    if name == LOCAL {
        form.remove("node");
        form.remove("repo");
        if let Some(remote) = remote {
            form.insert("remote".into(), json!(remote));
        }
    } else {
        form.insert("node".into(), json!(name));
    }
    Ok(Some((Value::Object(form).to_string(), name)))
}

type Answer = std::result::Result<Value, String>;

/// What the hub last heard from a node, and whether it is asking right now.
#[derive(Default)]
struct Entry {
    /// The ssh target the answer was fetched from, so a changed node starts over.
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
fn state_of(home: &Home, node: &Node) -> Answer {
    let identity = node.ssh.clone();
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
    let answer = fetch(home, node, "/api/state?local=1")
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
                { "name": LOCAL, "ssh": null, "online": true },
                { "name": "nodes.toml", "ssh": "", "online": false, "error": format!("{e:#}") },
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
        let handles: Vec<_> = nodes
            .iter()
            .map(|n| s.spawn(|| state_of(home, n)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("crashed".into())))
            .collect()
    });
    let mut summary = vec![json!({ "name": LOCAL, "ssh": null, "online": true, "writable": true })];
    for (node, answer) in nodes.iter().zip(answers) {
        match answer {
            Ok(mut theirs) => {
                for list in LISTS {
                    tag_rows(&mut theirs[list], &node.name, true);
                    if let (Some(rows), Some(mine)) =
                        (theirs[list].as_array().cloned(), state[list].as_array_mut())
                    {
                        mine.extend(rows);
                    }
                }
                add_counts(&mut state["counts"], &theirs["counts"]);
                summary.push(
                    json!({ "name": node.name, "ssh": node.ssh, "online": true, "writable": true }),
                );
            }
            Err(error) => summary.push(
                json!({ "name": node.name, "ssh": node.ssh, "online": false, "error": error }),
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
            // Every node can be changed over ssh; older pages look for this.
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
    use std::sync::Arc;

    type Fake = Arc<dyn Fn(bool, &str, &str) -> Result<(u16, String)> + Send + Sync>;

    fn fakes() -> &'static Mutex<HashMap<String, Fake>> {
        static FAKES: OnceLock<Mutex<HashMap<String, Fake>>> = OnceLock::new();
        FAKES.get_or_init(Default::default)
    }

    /// The stand-in for a node's ssh target, if `target` is one: what
    /// `exchange` asks instead of running ssh.
    pub(super) fn fake(target: &str) -> Option<Fake> {
        fakes().lock().unwrap().get(target).cloned()
    }

    /// A fresh ssh target whose requests go to `answer`.
    fn register(answer: Fake) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let target = format!(
            "fake-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        fakes().lock().unwrap().insert(target.clone(), answer);
        target
    }

    fn home() -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::resolve(Some(dir.path().to_path_buf())).unwrap();
        (dir, home)
    }

    #[test]
    fn nodes_round_trip_and_an_old_url_node_says_what_to_do() {
        let (_dir, home) = home();
        assert!(load(&home).unwrap().is_empty());
        let node = Node {
            name: "box".into(),
            ssh: "me@box".into(),
        };
        save(&home, vec![node.clone()]).unwrap();
        assert_eq!(load(&home).unwrap(), vec![node]);
        std::fs::write(
            path(&home),
            "[[node]]\nname = \"box\"\nurl = \"http://box:7433\"\ntoken = \"t\"\n",
        )
        .unwrap();
        let err = format!("{:#}", load(&home).unwrap_err());
        assert!(err.contains("furmi node add"), "{err}");
    }

    #[test]
    fn names_are_ids_and_not_the_local_one() {
        let node = |name: &str| Node {
            name: name.into(),
            ssh: "box".into(),
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

    fn cap(remotes: &[&str], agents: &[&str], running: usize, max: Option<usize>) -> Capacity {
        Capacity {
            remotes: remotes.iter().map(|s| s.to_string()).collect(),
            agents: agents.iter().map(|s| s.to_string()).collect(),
            running,
            runner_alive: true,
            max_parallel: max,
            max_parallel_per_repo: max.map(|_| 1),
            ..Capacity::default()
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
        assert_eq!(choose(&all, Some(app), None).unwrap(), "box");
        // Only box has codex.
        assert_eq!(choose(&all, Some(app), Some("codex")).unwrap(), "box");
        let err = choose(&all, Some(app), Some("pi")).unwrap_err().to_string();
        assert!(err.contains("`pi`"), "{err}");
        let err = choose(&all, Some("github.com/me/none"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no machine"), "{err}");
    }

    #[test]
    fn auto_prefers_a_free_slot_then_this_machine_on_a_tie() {
        let app = "github.com/me/app";
        // box is idle but full; local has a slot despite running more.
        let all = vec![
            ("local".to_string(), cap(&[app], &["claude"], 1, Some(2))),
            ("box".to_string(), cap(&[app], &["claude"], 0, Some(0))),
        ];
        assert_eq!(choose(&all, Some(app), None).unwrap(), "local");
        // Full everywhere: still placed, on the least loaded.
        let full = vec![
            ("local".to_string(), cap(&[app], &["claude"], 3, Some(3))),
            ("box".to_string(), cap(&[app], &["claude"], 2, Some(2))),
        ];
        assert_eq!(choose(&full, Some(app), None).unwrap(), "box");
        // A tie goes to the first listed.
        let tie = vec![
            ("local".to_string(), cap(&[app], &["claude"], 1, Some(3))),
            ("box".to_string(), cap(&[app], &["claude"], 1, Some(3))),
        ];
        assert_eq!(choose(&tie, Some(app), None).unwrap(), "local");
    }

    #[test]
    fn auto_counts_waiting_tasks_and_skips_machines_that_cannot_start_work() {
        let app = "github.com/me/app";
        let mut busy = cap(&[app], &["claude"], 0, Some(3));
        busy.queued = 5;
        let all = vec![
            ("local".to_string(), busy),
            ("box".to_string(), cap(&[app], &["claude"], 1, Some(3))),
        ];
        assert_eq!(choose(&all, Some(app), None).unwrap(), "box");
        // No runner, or no word of its limits (an older furmi): never picked.
        let mut stopped = cap(&[app], &["claude"], 0, Some(3));
        stopped.runner_alive = false;
        let old = Capacity {
            max_parallel: None,
            ..cap(&[app], &["claude"], 0, None)
        };
        let all = vec![
            ("stopped".to_string(), stopped),
            ("old".to_string(), old),
            ("box".to_string(), cap(&[app], &["claude"], 2, Some(3))),
        ];
        assert_eq!(choose(&all, Some(app), None).unwrap(), "box");
        // No agent installed there.
        let all = vec![("box".to_string(), cap(&[app], &[], 0, Some(3)))];
        let err = choose(&all, Some(app), None).unwrap_err().to_string();
        assert!(err.contains("agent"), "{err}");
    }

    #[test]
    fn auto_respects_the_per_repo_limit() {
        let app = "github.com/me/app";
        let mut local = cap(&[app], &["claude"], 1, Some(3));
        local.running_in.insert(app.into(), 1);
        // Two running, but in other repos: app has room.
        let box_ = cap(&[app], &["claude"], 2, Some(3));
        let all = vec![("local".to_string(), local), ("box".to_string(), box_)];
        assert_eq!(choose(&all, Some(app), None).unwrap(), "box");
    }

    #[test]
    fn a_scratch_task_goes_to_the_least_busy_machine_clone_or_not() {
        let all = vec![
            ("local".to_string(), cap(&[], &["claude"], 2, Some(3))),
            (
                "box".to_string(),
                cap(&["github.com/me/app"], &["claude"], 0, Some(3)),
            ),
        ];
        assert_eq!(choose(&all, None, None).unwrap(), "box");
        assert!(choose(&[], None, None).is_err());
        let (_dir, home) = home();
        let form = r#"{"prompt":"p","scratch":true,"node":"auto"}"#;
        let local = cap(&[], &["claude"], 0, Some(3));
        let (body, name) = resolve_auto(&home, || Ok(local), form).unwrap().unwrap();
        assert_eq!(name, "local");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert!(body.get("node").is_none() && body.get("remote").is_none());
        assert_eq!(body["scratch"], true);
    }

    #[test]
    fn a_scratch_task_for_another_node_names_no_project() {
        let (_dir, home) = home();
        let (url, request) = capture("200 OK", r#"{"ok":true,"id":"t"}"#);
        with_node(&home, "box", url);
        let form = r#"{"prompt":"p","scratch":true,"node":"box"}"#;

        let (status, _) = proxy(&home, true, "/api/tasks", form).unwrap();

        assert_eq!(status, 200);
        let sent = request.recv().unwrap();
        let body: Value = serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["scratch"], true);
        assert!(body.get("remote").is_none() && body.get("node").is_none());
    }

    #[test]
    fn a_node_cannot_be_called_auto() {
        let node = Node {
            name: "auto".into(),
            ssh: "box".into(),
        };
        assert!(check(&node).is_err());
    }

    #[test]
    fn auto_forms_are_rewritten_for_the_chosen_machine() {
        let (_dir, home) = home();
        let app = "github.com/me/app";
        let form = format!(r#"{{"prompt":"p","repo":"{app}","node":"auto"}}"#);
        // Not auto: left alone.
        assert!(
            resolve_auto(&home, || Ok(Capacity::default()), r#"{"node":"box"}"#)
                .unwrap()
                .is_none()
        );
        // Only this machine has it: the form becomes a local one, by remote.
        let local = cap(&[app], &["claude"], 0, Some(3));
        let (body, name) = resolve_auto(&home, || Ok(local), &form).unwrap().unwrap();
        assert_eq!(name, "local");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["remote"], app);
        assert!(body.get("node").is_none() && body.get("repo").is_none());
        // Nobody has it.
        let err = resolve_auto(&home, || Ok(Capacity::default()), &form)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no machine"), "{err}");
    }

    #[test]
    fn auto_asks_the_other_machines_what_they_have() {
        let (_dir, home) = home();
        let url = serve(
            r#"{"repos":[{"remote":"github.com/me/app","path":"~/app"}],"agents":["claude"],"agent":"claude","running":0,"queued":0,"running_in":{},"runner_alive":true,"max_parallel":3,"max_parallel_per_repo":1}"#,
            "200 OK",
        );
        save(
            &home,
            vec![
                Node {
                    name: "box".into(),
                    ssh: url,
                },
                // Down: skipped, not an error.
                Node {
                    name: "gone".into(),
                    ssh: down(),
                },
            ],
        )
        .unwrap();
        let busy = cap(&["github.com/me/app"], &["claude"], 3, Some(3));
        let form = r#"{"prompt":"p","repo":"github.com/me/app","node":"auto"}"#;
        let (body, name) = resolve_auto(&home, || Ok(busy), form).unwrap().unwrap();
        assert_eq!(name, "box");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["node"], "box");
        assert_eq!(body["repo"], "github.com/me/app");
    }

    /// A node that answers `body` to every request, once per connection.
    /// A node that answers every request with `status` and `body`.
    fn serve(body: &'static str, status: &'static str) -> String {
        let code: u16 = status.split(' ').next().unwrap().parse().unwrap();
        register(Arc::new(move |_, _, _| Ok((code, body.to_string()))))
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
                    ssh: up,
                },
                Node {
                    name: "refused".into(),
                    ssh: refused,
                },
                // Nothing listens on port 9 of loopback.
                Node {
                    name: "gone".into(),
                    ssh: down(),
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

    /// A node whose every request fails.
    fn down() -> String {
        register(Arc::new(|_, _, _| {
            Err(anyhow::anyhow!("connection refused"))
        }))
    }

    fn node(name: &str, url: String) -> Node {
        Node {
            name: name.into(),
            ssh: url,
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
    fn a_changed_target_does_not_get_the_old_answer() {
        let (_dir, home) = home();
        let url = serve(r#"{"queue":[]}"#, "200 OK");
        let mut n = node("cache-test", url);
        assert!(state_of(&home, &n).is_ok());
        n.ssh = down();
        assert!(state_of(&home, &n).is_err());
    }

    #[test]
    fn the_remote_path_survives_the_remote_shell() {
        assert_eq!(shell_quote("/api/state?a=1&b=2"), "'/api/state?a=1&b=2'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn the_control_path_is_escaped_or_left_out() {
        let ok = control_options(Path::new("/home/me/.furmiculi/ssh"));
        assert!(ok.contains(&"ControlPath=\"/home/me/.furmiculi/ssh/%C\"".to_string()));
        let pct = control_options(Path::new("/home/100%/ssh"));
        assert!(pct.iter().any(|o| o.contains("100%%/ssh")), "{pct:?}");
        for bad in ["/a\"b/ssh", &format!("/{}/ssh", "x".repeat(60))] {
            assert_eq!(control_options(Path::new(bad)), ["-o", "ControlMaster=no"]);
        }
    }

    #[test]
    fn odd_ssh_targets_are_refused() {
        for bad in ["", "-oProxyCommand=x", "box other", "box\n", "-J box"] {
            assert!(check(&node("box", bad.into())).is_err(), "{bad:?}");
        }
        for good in ["box", "me@box.ts.net", "ssh://me@box:2222"] {
            assert!(check(&node("box", good.into())).is_ok(), "{good}");
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

    /// A node that records the request it gets (as `METHOD path`, a blank
    /// line, then the body) and answers `status` + `body`.
    fn capture(
        status: &'static str,
        body: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let code: u16 = status.split(' ').next().unwrap().parse().unwrap();
        let target = register(Arc::new(move |post, path, sent| {
            let method = if post { "POST" } else { "GET" };
            let _ = tx
                .lock()
                .unwrap()
                .send(format!("{method} {path}\r\n\r\n{sent}"));
            Ok((code, body.to_string()))
        }));
        (target, rx)
    }

    fn with_node(home: &Home, name: &str, url: String) {
        save(
            home,
            vec![Node {
                name: name.into(),
                ssh: url,
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
            sent.starts_with("POST /api/tasks/fix-ci/fail\r\n"),
            "{sent}"
        );
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
    fn a_schedule_for_another_node_is_created_there_by_remote() {
        let (_dir, home) = home();
        let (url, request) = capture("200 OK", r#"{"ok":true,"id":"s"}"#);
        with_node(&home, "box", url);
        let form = r#"{"prompt":"p","repo":"github.com/me/app","cron":"0 3 * * *","node":"box"}"#;

        let (status, _) = proxy(&home, true, "/api/schedules", form).unwrap();

        assert_eq!(status, 200);
        let sent = request.recv().unwrap();
        assert!(sent.starts_with("POST /api/schedules\r\n"), "{sent}");
        let body: Value = serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["remote"], "github.com/me/app");
        assert_eq!(body["cron"], "0 3 * * *");
        assert!(body.get("repo").is_none() && body.get("node").is_none());
    }

    #[test]
    fn this_machines_own_requests_are_not_proxied() {
        let (_dir, home) = home();
        with_node(&home, "box", down());
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
        with_node(&home, "b", down());
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
        with_node(&home, "box", down());
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
    fn a_plain_text_answer_comes_back_as_a_readable_json_error() {
        let (_dir, home) = home();
        let (url, _request) = capture("403 Forbidden", "forbidden: nope");
        with_node(&home, "box", url);

        let (status, body) = proxy(&home, true, "/api/tasks/a%40box/cancel", "{}").unwrap();

        assert_eq!(status, 403);
        let error = serde_json::from_str::<Value>(&body).unwrap()["error"].to_string();
        assert!(error.contains("forbidden: nope"), "{error}");
    }
}
