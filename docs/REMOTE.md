# Serving the dashboard to other machines

By default `furmi web` and `furmi start` bind to 127.0.0.1 only: the dashboard
exists in your machine's loopback, checked with a loopback `Host` requirement
(which defeats DNS rebinding), a per-run token POSTs must carry, and a loopback
`Origin`. On a box without a screen there's nothing to open 127.0.0.1, so the
dashboard can serve your other machines instead.

The **first** `furmi web` asks about that, once:

```sh
$ furmi web
Setting up the dashboard; this is saved, so it only asks once.
Serve other machines (your tailnet), not just this one? (y/n) [y]: y
Host name(s) browsers use to reach this box, comma-separated [box.tailnet.ts.net, 100.64.0.10]:
```

It saves the answers as a `[web]` table in `config.toml` and creates
`~/.furmiculi/web-token` (0600, 128 random bits), then serves — as a wide,
token-gated bind. Where tailscale can't be asked what this box is called (in a
container whose `tailscaled` runs on the host, say) there's no name to fill in
and the default is `*`: enter accepts any Host, or type the name your browsers
use, and `furmi web --setup` takes it later. Answering "no" (or pressing enter) is saved the same way, as
`configured = true` with nothing else, and keeps the loopback-only default. It
only ever asks on a terminal: started from a unit or a script, it serves
loopback silently rather than waiting for an answer. `furmi web --setup` asks
again whenever you change your mind, and prefills what it knows.

After that, a plain `furmi web` — no flags, no questions — does the same thing
every time, and `furmi start`/`furmi service` follow the same table.
`furmi doctor` prints the URL to open.

Open the URL it prints once, with the token in the query string:

```
http://box.tailnet.ts.net:7433/?token=<the token in ~/.furmiculi/web-token>
```

The server trades the token for a session: it answers with a redirect to the
same page without `?token=` (so the secret never stays in the address bar, the
history or a bookmark) and an `HttpOnly`, `SameSite=Strict` cookie,
`furmi_session`, holding a random session id, not the token. The page is
served its session's own token to send with every request, not the master one.

A session lasts 15 minutes past its last request; every request, and a
keepalive the open page sends every 5 minutes, pushes it out again. Close the
tab and it is gone a quarter of an hour later; open the URL with the token
again for a new one. Sessions live in the server's memory, so restarting
`furmi web` ends them all, and that is also how a rotated token file takes
effect: the old token and every session made with it stop working.

The cookie gets `Secure` when the request arrives over TLS, which the server
itself doesn't speak: put a TLS-terminating proxy in front (`tailscale serve`,
Caddy, nginx) that sends `X-Forwarded-Proto: https`. A plain-HTTP bind, which is
what a LAN or tailnet address without a proxy is, can't have `Secure`; the
session is still short-lived and `HttpOnly`, but anyone who can watch the
network can read it. The per-run token and Origin checks POSTs already had stay
in place on top of that. (Older versions kept the token itself in a 30-day
cookie; the first page load after upgrading tells the browser to delete it.)
The URL with the token in it is printed only on a terminal; a unit's journal
gets `?token=<the token>` instead, so the secret doesn't sit in logs.

Everything stays overridable by hand, and a flag always beats the saved value:

```sh
furmi web --listen 0.0.0.0:7433 --token-file ~/.furmiculi/web-token \
  --allowed-host box.tailnet.ts.net --no-open
```

- `--listen <addr>`: the address to bind (`0.0.0.0:7433` for every
  interface).
- `--token-file <path>` (or `--token <value>`, but the file keeps the secret
  out of argv, shell history and `ps` — which is also why `--token` is refused
  together with `-d`/`--detach`, where the backgrounded dashboard's command
  line would carry it for as long as it runs): a shared secret every request
  must carry — `Authorization: Bearer <t>`. It must be 0600; a group- or
  world-readable one is refused.
- `--allowed-host <name>` (repeatable): the exact `Host` the browser sends
  when it reaches the dashboard; requests with any other Host are rejected.
  An entry alone allows any port on that host; `host:port` pins the port. `*`
  instead accepts any Host — for when the name isn't knowable up front (the
  setup prompt takes it too) — and then the bearer token is the only gate,
  so keep it 0600.

Starting a second dashboard on an address that's already served doesn't just
fail with "address already in use": the serving one is recorded (pid and
address, in `~/.furmiculi/.web.lock`), so the new one offers to stop it and
take over — or does it without asking, with `--replace`, for scripts and unit
files. Anything that isn't this home's own dashboard is left alone, with the
old error.

The dashboard speaks plain HTTP: put `tailscale serve` in front of it if you
want HTTPS. And the quietest pattern is still SSH:
`ssh -L 7433:127.0.0.1:7433 your-box`, with `furmi web` keeping every original
protection untouched.

## Showing several machines in one dashboard

Each machine runs its own furmi; one dashboard can show them all. On the
machine you open, add the others as nodes:

```sh
furmi node add box box.tailnet.ts.net     # or me@box, or a Host from ~/.ssh/config
furmi node ls                    # each node, and whether it answers
furmi node rm box
```

A node is an SSH target, the way herdr saves its machines: anything `ssh`
takes, and it must work without furmi's help (`ssh box` logs in). furmi runs
`furmi node call` there, which answers the dashboard's own routes in-process,
so the other machine needs **furmi installed** (0.5 or later, at
`~/.cargo/bin` or `~/.local/bin` or on the PATH of a non-interactive login) but
no open port, no running dashboard and no token. Your keys, agent and
`known_hosts` do the authenticating and the encrypting. `node add` connects
once before saving anything, so it can ask for a passphrase or an unknown host
key; if that fails, nothing is saved. The connection is then shared for ten
minutes (OpenSSH `ControlPersist`, sockets in `~/.furmiculi/ssh/`), so the
dashboard polling a node doesn't log in each time. Everything after that is
non-interactive (`BatchMode`): a node that now wants a password shows as
offline, and `furmi node add` again is how you sign in (a plain `ssh box` doesn't share furmi's connection). Nodes are
kept in `~/.furmiculi/nodes.toml`; it holds no secrets. `FURMI_SSH` names a
different `ssh` program.

When furmi isn't just `furmi` on that machine's PATH, say how to start it with
`--furmi`. The usual case is furmi running in a container there (your SSH login
reaches the host, not the container):

```sh
furmi node add pod box --furmi 'docker exec -i pod furmi'
```

The remote shell runs that command as written, so it needs `-i` to pass the
request through, and the login user must be allowed to run `docker`.

The dashboard then lists every machine's tasks and schedules with a **Node**
column, and the counts add up. A node that doesn't answer within 3 seconds is
named in a notice and its rows drop out (a failure is remembered for 10 seconds,
so a dead node isn't asked on every refresh); nothing else is affected. The same
project path on two machines is one project in the sidebar. You can act on other machines' rows from here: their buttons (done, fail, drop,
try again, edit, output) and the bulk bar work on a task wherever it runs, and
**+ New task** has a **Which machine?** choice. A task for another machine names
its project by git remote (`github.com/me/app`, the repo's `origin`): that
machine uses its own clone of it, and says so if it has none (it knows the
repos it was started in and has run tasks in, so run a task there, or start
`furmi web` in the clone, once). Its choices of
project and agent come from that machine. Ids are only unique per machine, so a
row from another machine has `@<node>` behind its id.

Choose **Any machine (least busy)** to let the dashboard place the task. At
creation it asks each machine that can be written to what it has, and takes
those whose runner is started and that have a clone of the project and an
agent (the one you picked, if any). Among them it prefers one with a free slot
for that project (`runner.max_parallel` and `max_parallel_per_repo`), then the
fewest running and waiting tasks, then this machine. If every slot is taken it
still picks the least loaded: the task waits its turn there. The choice is made
once, when the task is created, and the answer says where it went; it doesn't
move later. A machine that doesn't answer (or just failed to) is skipped, and
so is one on an older furmi, which doesn't report its load. Agents aren't
picked in the form for this choice: each machine uses its own default. `auto`
is a reserved node name.

A node running an older furmi (before ssh nodes) can't be reached at all;
upgrade it with `furmi update`.

A task with no project (`--scratch`, or **No project** in the form) needs no
clone anywhere, so any online machine can take it: *Any machine* weighs only
load and agent. Schedules can't be scratch tasks yet.

A repeating schedule can be placed the same way, by choosing a machine (or
*Any machine*) in **+ New task** with a repeat other than "Just once". The
schedule is created on that machine, which keeps it and fires it from then on,
so it keeps running when the machine you created it from is off. With *Any
machine* the pick is made once, when the schedule is created, and doesn't move
later; to move it, delete it and create it again. Times are read in that
machine's timezone, not yours: "daily at 09:00" fires at 09:00 where the
schedule lives. It shows in every dashboard
that has the machine added, with `@<node>` behind its id.
