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
/// How long a node gets to answer before it counts as offline.
const TIMEOUT: Duration = Duration::from_secs(3);
/// How long an answer (or a failure) is reused, so a page polling every
/// couple of seconds, or several open pages, ask a node once.
const CACHE_TTL: Duration = Duration::from_secs(2);
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
    use std::os::unix::fs::PermissionsExt;
    let path = path(home);
    let body = toml::to_string(&NodesFile { nodes })?;
    let text = format!("# Written by `furmi node`; holds each node's token.\n{body}");
    crate::fsutil::write_atomic(&path, text)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("securing {}", path.display()))
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
    let _lock = home.lock()?;
    let mut nodes = load(home)?;
    // Same name: replaced, so adding again is how a token is changed.
    nodes.retain(|n| n.name != node.name);
    nodes.push(node.clone());
    save(home, nodes)?;
    println!("added {}", node.name);
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
    for node in &nodes {
        let status = match fetch(node, "/api/state?local=1") {
            Ok(_) => "online".to_string(),
            Err(e) => format!("offline: {e:#}"),
        };
        println!("{:<16} {:<40} {status}", node.name, node.url);
    }
    Ok(())
}

fn check(node: &Node) -> Result<()> {
    validate_id(&node.name).map_err(anyhow::Error::msg)?;
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
    let (host, port, host_header) = parse_url(&node.url)?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("looking up {host}"))?
        .next()
        .with_context(|| format!("no address for {host}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).context("connecting")?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let auth = if node.token.is_empty() {
        String::new()
    } else {
        format!("Authorization: Bearer {}\r\n", node.token)
    };
    // HTTP/1.0: the answer is never chunked, so the body is whatever follows
    // the headers until the connection closes.
    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: {host_header}\r\n{auth}Accept: application/json\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    stream
        .take(MAX_BODY)
        .read_to_end(&mut raw)
        .context("reading the answer")?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").context("not an HTTP answer")?;
    let status = head.lines().next().unwrap_or_default();
    match status.split(' ').nth(1) {
        Some("200") => Ok(body.to_string()),
        Some("401") => bail!("refused the token (HTTP 401)"),
        Some("403") => bail!("refused the request (HTTP 403): is its host allowed on that node?"),
        _ => bail!("answered `{status}`"),
    }
}

type Answer = std::result::Result<Value, String>;

fn cache() -> &'static Mutex<HashMap<String, (Instant, Answer)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Answer)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// A node's `/api/state`, or why there isn't one, reused for a moment.
fn state_of(node: &Node) -> Answer {
    let key = format!("{}\n{}\n{}", node.name, node.url, node.token);
    if let Some((at, answer)) = cache().lock().unwrap().get(&key)
        && at.elapsed() < CACHE_TTL
    {
        return answer.clone();
    }
    let answer = fetch(node, "/api/state?local=1")
        .and_then(|body| serde_json::from_str(&body).context("not furmi's state"))
        .map_err(|e| format!("{e:#}"));
    cache()
        .lock()
        .unwrap()
        .insert(key, (Instant::now(), answer.clone()));
    answer
}

const LISTS: [&str; 4] = ["queue", "running", "archive", "schedules"];

/// `local` (this machine's `/api/state`) with every node's rows added, each row
/// tagged with its `node`, the counts summed, and a `nodes` list saying which
/// answered. With no nodes configured, `local` comes back untouched.
pub fn merge_state(home: &Home, local: String) -> String {
    let nodes = match load(home) {
        Ok(nodes) if !nodes.is_empty() => nodes,
        _ => return local,
    };
    let Ok(mut state) = serde_json::from_str::<Value>(&local) else {
        return local;
    };
    for list in LISTS {
        tag_rows(&mut state[list], LOCAL);
    }
    // In parallel: a node that doesn't answer costs one timeout, not one each.
    let answers: Vec<Answer> = std::thread::scope(|s| {
        let handles: Vec<_> = nodes.iter().map(|n| s.spawn(|| state_of(n))).collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("crashed".into())))
            .collect()
    });
    let mut summary = vec![json!({ "name": LOCAL, "url": null, "online": true })];
    for (node, answer) in nodes.iter().zip(answers) {
        match answer {
            Ok(mut theirs) => {
                for list in LISTS {
                    tag_rows(&mut theirs[list], &node.name);
                    if let (Some(rows), Some(mine)) =
                        (theirs[list].as_array().cloned(), state[list].as_array_mut())
                    {
                        mine.extend(rows);
                    }
                }
                add_counts(&mut state["counts"], &theirs["counts"]);
                summary.push(json!({ "name": node.name, "url": node.url, "online": true }));
            }
            Err(error) => summary.push(
                json!({ "name": node.name, "url": node.url, "online": false, "error": error }),
            ),
        }
    }
    state["nodes"] = Value::Array(summary);
    state.to_string()
}

fn tag_rows(rows: &mut Value, node: &str) {
    for row in rows.as_array_mut().into_iter().flatten() {
        row["node"] = json!(node);
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
        assert_eq!(merged["queue"][1]["id"], "b");
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
}
