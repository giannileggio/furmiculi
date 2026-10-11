# loompa-forge

Queue and run coding-agent tasks defined as Markdown files.

- **Tasks** are `.md` files: YAML frontmatter says where and how to run
  (repo, branch, worktree, agent, model, schedule...), and the body is the
  prompt.
- **Schedules** are task templates with a cron expression. loompa-forge
  enqueues a new task each time one fires.
- **`lf`** runs pending tasks in tmux, tracks their status in the file, and
  moves finished tasks to `archive/`.

It works with any CLI coding agent: presets for Claude Code, Codex, Gemini
CLI, opencode and Pi are built in, and others are a few lines of config.

Because the format is plain Markdown, you can write tasks by hand, with
`lf add`, or by asking an agent: open it in `~/.loompa-forge` and say "add a
task to fix the login redirect in ~/Projects/myapp tonight, open a PR". `lf
init` puts an `AGENTS.md` and an `lf-tasks` skill there, which teach the agent
to write a self-contained prompt and queue it with `lf add`. It can also link
the skill into your global skill folders, so you can queue work from any repo.

## Quick start

```sh
# 1. install (Linux or macOS; needs git and tmux)
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/giannileggio/loompa-forge/releases/latest/download/loompa-forge-installer.sh | sh

# 2. queue some work, from inside your project
cd ~/Projects/myapp
lf add "Fix the redirect loop after login"

# 3. run it (starts the runner and opens the dashboard)
lf start
```

That's all: the first command you run sets up `~/.loompa-forge` and picks the
coding agent it finds on your `PATH`, and `lf add` works on the git repo you're
standing in. In the dashboard, **+ New task** queues more work (once or on a
schedule), **Start the runner** turns it on, and **Setup** helps you pick and
install a coding agent and says what else is missing, all without the terminal.
No agent installed yet? `lf agents` lists them with install commands. Run `lf` on its own any time to see where things stand and what to
try next, and `lf doctor` if something isn't running.

## Status

Working now: `init`, `add`, `ls`, `validate`, `start`, `stop`, `agents`, `lf run` (the runner and
scheduler: tmux windows, git worktrees, retries, timeouts, `on_finish`),
`done|fail|cancel|retry|attach`, `logs`, `clean`, `web` (a dashboard you can
also act from), `status`/`doctor`/`service` (keeping the runner healthy), and
the agent skill for writing tasks.

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/giannileggio/loompa-forge/releases/latest/download/loompa-forge-installer.sh | sh
lf init            # optional: the first `lf add`/`lf start` sets up ~/.loompa-forge by
                   # itself (override with $LF_HOME or --home). `lf init` also asks
                   # whether to install the skill globally, so agents can queue tasks
```

Prebuilt binaries are published for Linux and macOS (x86_64 and arm64); the
script above fetches the right one and puts `lf` on your `PATH`. It also
installs `loompa-forge-update`, so `loompa-forge-update` upgrades an
existing install, and `lf` itself prints a one-line notice when a newer
version is out (set `LF_NO_UPDATE_CHECK=1` to turn that off).

Building from source instead:

```sh
cargo install --path .
```

## Usage

```sh
lf add "Fix the redirect loop after login"   # in the repo you're in; asks if you leave the text out
lf add --repo ~/Projects/myapp --on-finish pr --in 2h \
       "Fix the redirect loop after login"
echo "Update deps and run tests" | lf add --repo ~/Projects/myapp --agent codex

lf start             # the runner and the dashboard together (--no-open: no browser)
lf stop              # stop the runner (work already underway keeps going in tmux)
lf agents            # which coding agents are installed, and how to install the rest
lf agents use codex  # make one the default for new tasks

lf ls                # queue
lf ls --schedules    # schedules with their next run
lf ls --archive      # finished tasks
lf ls --watch        # ...live, every --interval (default 2s); in a terminal it's
                     # interactive: j/k move, l logs, d/f/c done/fail/cancel (asks
                     # first), r retry, a attach, tab queue/archive, q quit
lf validate          # check every task and schedule file

lf run               # run the queue until Ctrl-C (rescans every poll_interval)
lf run --once        # one pass: enqueue due schedules, reap finished tasks, start new ones
lf attach [id]       # watch running tasks in tmux, one window each
lf logs <id>         # print a task's output: live if running, else the saved log
lf logs <id> --attempt 1   # an earlier attempt, instead of the latest

lf done [id]         # finish a task: runs on_finish, archives it
lf fail [id] -r why  # give up on it, without retrying
lf cancel [id]       # drop a pending or running task
lf retry <id>        # requeue a failed/cancelled/needs_review task

lf clean             # remove worktrees of done tasks (-n: dry run)
lf clean --all --older-than 7d   # ...and of failed/cancelled ones, a week on
lf clean --archive --all --older-than 30d   # ...and their archived task files/logs

lf status            # is the runner alive? queue, spend, failing schedules
lf doctor            # check tmux/git/agents/config/task files; non-zero on problems
lf service systemd   # print a unit that keeps `lf run` running (or: launchd)

lf web               # dashboard at http://127.0.0.1:7433, until Ctrl-C
lf web --port 8080   # a different port
lf web --listen 0.0.0.0:7433 --token-file ~/.loompa-forge/web-token \
  --allowed-host tools.example   # on a headless box, reached from a tailnet (see below)
```

Inside a task's window, `id` defaults to that task, so an interactive
session ends with a plain `lf done`, run by you or by the agent.

`lf run` starts each task in a window of the `loompa` tmux session, inside
`~/.loompa-forge/worktrees/<id>` (a git worktree on the task's branch) unless
`worktree: false`. It respects `max_parallel` and `max_parallel_per_repo`,
retries failures after `retry_delay`, kills attempts that exceed `timeout`,
runs `on_finish` on success, and moves finished tasks to `archive/`. Each
attempt's output is saved to `logs/<id>.<attempt>.log`.

## Keeping it running

`lf run` is a plain foreground process; `lf service systemd` (Linux) or
`lf service launchd` (macOS) prints a unit file, with install instructions,
that starts it at login and restarts it if it dies. Only one runner per home
is allowed. `lf status` shows whether it's alive and when its last pass was
(it warns if the runner looks stuck), and `lf run` also appends to
`runner.log` in the home folder.

What `lf` does so that a bad day doesn't lose work:

- **Crash-safe state.** Task, schedule and exit files are replaced atomically,
  never left half-written.
- **Nothing can hang it.** `git`, `gh` and `tmux` calls have timeouts and
  can't prompt; `on_finish` (push, PR) runs outside the lock the other
  commands share, and waiting for that lock gives up after two minutes.
- **Reboots aren't failures.** A task whose tmux window vanished is requeued
  at once without using a retry. Headless output is streamed to the log file
  as it's produced, so it survives tmux dying too.
- **Timeouts really stop the agent**: SIGTERM to its process group, then
  SIGKILL.
- **Guard rails.** A schedule won't enqueue a new run while its last one is
  still queued (unless `allow_overlap: true`); `daily_budget_usd` stops new
  tasks once the day's reported cost reaches it.
- **Safe git handling.** A reused worktree must belong to the repo, `lf`
  won't switch your checkout's branch over uncommitted changes, and
  `on_finish: pr` won't open a second PR for a branch that has one.

`lf web` serves a local dashboard (queue, running, archive and schedules,
refreshed every couple of seconds) — a Sidekiq-style view of the same state
`lf ls` prints. **+ New task** queues work from a form (what to do, which
project, when to start, what to do with the result), calling the same code as
`lf add`; `lf start` is `lf web` plus a runner that stops with it. If the runner is off,
a banner says so with a **Start the runner** button (that runner keeps going
after you close the dashboard; `lf stop` or the **stop** link next to
"runner: on" ends it), and **Setup** lists the coding agents (installed or not,
with the install command to copy, and **Use this one** to make one the default)
above what `lf doctor` finds (tmux, git, task files), with a banner when
something needs attention. lf doesn't run the installs for you: they're global
`npm install -g` commands and each agent needs you to sign in once.

### On a headless box (your tailnet)

By default `lf web` and `lf start` bind to 127.0.0.1 only: the dashboard exists
in your machine's loopback, checked with a loopback `Host` requirement (which
defeats DNS rebinding), a per-run token POSTs must carry, and a loopback
`Origin`. On a server without a screen there's nothing to open 127.0.0.1, so
`--listen` names the address to bind instead (`0.0.0.0:7433` for every
interface). A non-loopback bind refuses to start without two more things:

- `--token-file <path>` (or `--token <value>`, but the file keeps the secret
  out of argv, shell history and `ps`): a shared secret every request must
  carry — `Authorization: Bearer <t>`. Make the file 0600 and put a long
  random string in it, e.g. `openssl rand -hex 16 > ~/.loompa-forge/web-token`.
- `--allowed-host <name>` (repeatable): the exact `Host` the browser sends
  when it reaches the dashboard; requests with any other Host are rejected.
  An entry alone allows any port on that host; `host:port` pins the port.

The first page load may pass the token as `?token=<t>`; the page keeps it in
memory only (never localStorage or a cookie) and strips it from the URL, then
sends it with every request. The per-run token and Origin checks POSTs already
had stay in place on top of that.

On the headless box:

```sh
lf web --listen 0.0.0.0:7433 --token-file ~/.loompa-forge/web-token \
  --allowed-host tools-your-hostname.ts.net --no-open
```

From another machine on the tailnet, open
`http://tools-your-hostname.ts.net:7433/?token=<the file's content>` once;
from then on `http://tools-your-hostname.ts.net:7433` works on its own while
that browser tab is open. (`lf start` takes the same flags, and its runner
stops with the dashboard, as always.) The dashboard speaks plain HTTP: put
`tailscale serve` in front of it if you want HTTPS. And the quietest pattern
is still SSH: `ssh -L 7433:127.0.0.1:7433 your-box`, with `lf web` keeping
every original protection untouched.

A task that is still *Waiting* has an **Edit** button: change what it does, the
project, when it starts, or what happens when it's done (not once it has
started: cancel it and add a new one). **+ New task** with "How often?" set to
anything but "Just once" creates a schedule, which appears under *Repeating*
with **Edit**, **Pause**/**Resume** and **Delete**. Changing a schedule's time or
resuming it starts counting from now, so it never fires for a slot that passed
meanwhile. The dashboard writes the same Markdown files you would, so
fields it doesn't show (`branch`, `model`, `allow_overlap`...) are kept.
The dashboard uses plainer words than the files do: *Waiting*, *Working*,
*Finished*, *Repeating*, *Needs review* (`pending`, `running`, the archive,
schedules, `needs_review`), and a failed task shows why it failed. Each task row offers the buttons that make sense for its
status (done, fail, cancel, retry) plus a log viewer, calling the same code
as `lf done|fail|cancel|retry` and `lf logs`. It only binds to
`127.0.0.1`, so anyone who can reach it could already run `lf` locally
themselves; there's no login. But a web page you visit can send requests to
`127.0.0.1` too, so the server only answers loopback `Host` names (against DNS
rebinding) and accepts actions only with a random per-run token that just the
dashboard page itself can read (against cross-site forgery). A task or
schedule file that can't be parsed shows up in a warning box instead of
breaking the page.

See [docs/FORMAT.md](docs/FORMAT.md) for the file formats.

See [TODO.md](TODO.md) for open work and ideas.

## Testing

```sh
cargo test                  # unit tests, plus the CLI and runner tests in tests/
                            # (they run the built `lf` against real git and tmux)

cargo build                 # browser tests for the dashboard, in e2e/
cd e2e && npm ci && npx playwright install chromium
npm test                    # LF_BIN=path/to/lf to test another build
```

The browser tests start `lf start`/`lf web` for each test against a throwaway
home, git repo and tmux server, with a stub agent that runs the prompt as a
shell command, so they need `tmux` and `git` but no real coding agent and touch
none of your files. CI runs both on every push and pull request.
