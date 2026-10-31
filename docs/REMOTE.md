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

The server answers with an `HttpOnly`, `SameSite=Strict` cookie holding the
token, and the page keeps it in memory only (never localStorage) and strips it
from the URL, then sends it with every request. The cookie is what makes a
reload — or a bookmark — work after the token is gone from the address bar;
without it every refresh would be a 401. The per-run token and Origin checks
POSTs already had stay in place on top of that, so the cookie alone can't
change anything. It lasts 30 days, re-set on each page load. (The URL with the
token in it is printed only on a terminal; a unit's journal gets
`?token=<the token>` instead, so the secret doesn't sit in logs.)

Everything stays overridable by hand, and a flag always beats the saved value:

```sh
furmi web --listen 0.0.0.0:7433 --token-file ~/.furmiculi/web-token \
  --allowed-host box.tailnet.ts.net --no-open
```

- `--listen <addr>`: the address to bind (`0.0.0.0:7433` for every
  interface).
- `--token-file <path>` (or `--token <value>`, but the file keeps the secret
  out of argv, shell history and `ps`): a shared secret every request must
  carry — `Authorization: Bearer <t>`. It must be 0600; a group- or
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
furmi node add box http://box.tailnet.ts.net:7433 --token-file ~/box-web-token
furmi node ls                    # each node, and whether it answers
furmi node rm box
```

The URL is where that machine serves `furmi web` (above), and the token is its
`~/.furmiculi/web-token`; `http://` only, since the dashboard speaks plain HTTP
(a tailnet or an SSH tunnel such as `http://127.0.0.1:7434` already encrypts
it). Nodes are kept in `~/.furmiculi/nodes.toml`, 0600 because of the tokens.

The dashboard then lists every machine's tasks and schedules with a **Node**
column, and the counts add up. A node that doesn't answer within 3 seconds is
named in a notice and its rows drop out (a failure is remembered for 10 seconds,
so a dead node isn't asked on every refresh); nothing else is affected. The same
project path on two machines is one project in the sidebar. `furmi node add`
warns when the URL isn't loopback or a tailnet address, since the token travels
in the clear. 
You can act on other machines' rows from here: their buttons (done, fail, drop,
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

Changing things on a node needs its token: a node added without one (a loopback
address, say) can be read but not changed, and its rows show no buttons. A node
running an older furmi can be read, but can't take tasks by remote; upgrade it. Repeating tasks can't be placed on
another machine yet.
