# furmiculi

Queue and run coding-agent tasks defined as Markdown files.

- **Tasks** are `.md` files: YAML frontmatter says where and how to run
  (repo, branch, worktree, agent, model, schedule...), and the body is the
  prompt.
- **Schedules** are task templates with a cron expression. furmiculi
  enqueues a new task each time one fires.
- **`furmi`** runs pending tasks in tmux, tracks their status in the file, and
  moves finished tasks to `archive/`.

It works with any CLI coding agent: presets for Claude Code, Codex, Gemini
CLI, opencode and Pi are built in, and others are a few lines of config.

Because the format is plain Markdown, you can write tasks by hand, with
`furmi add`, or by asking an agent: open it in `~/.furmiculi` and say "add a
task to fix the login redirect in ~/Projects/myapp tonight, open a PR". `furmi
init` puts an `AGENTS.md` and an `furmi-tasks` skill there, which teach the agent
to write a self-contained prompt and queue it with `furmi add`. It can also link
the skill into your global skill folders, so you can queue work from any repo.

## Quick start

```sh
# 1. install (Linux or macOS; needs git and tmux)
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/giannileggio/furmiculi/releases/latest/download/furmiculi-installer.sh | sh

# 2. queue some work, from inside your project
cd ~/Projects/myapp
furmi add "Fix the redirect loop after login"

# 3. run it (starts the runner and opens the dashboard)
furmi start
```

That's all: the first command you run sets up `~/.furmiculi` and picks the
coding agent it finds on your `PATH`, and `furmi add` works on the git repo you're
standing in. In the dashboard, **+ New task** queues more work (once or on a
schedule), **Start the runner** turns it on, and **Setup** helps you pick and
install a coding agent and says what else is missing, all without the terminal.
No agent installed yet? `furmi agents` lists them with install commands. Run `furmi` on its own any time to see where things stand and what to
try next, and `furmi doctor` if something isn't running.

## Status

Working now: `init`, `add`, `ls`, `validate`, `start`, `stop`, `agents`, `furmi run` (the runner and
scheduler: tmux windows, git worktrees, retries, timeouts, `on_finish`),
`done|fail|cancel|retry|attach`, `logs`, `clean`, `web` (a dashboard you can
also act from), `status`/`doctor`/`service` (keeping the runner healthy), and
the agent skill for writing tasks.

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/giannileggio/furmiculi/releases/latest/download/furmiculi-installer.sh | sh
furmi init            # optional: the first `furmi add`/`furmi start` sets up ~/.furmiculi by
                   # itself (override with $FURMI_HOME or --home). `furmi init` also asks
                   # whether to install the skill globally, so agents can queue tasks
```

Prebuilt binaries are published for Linux and macOS (x86_64 and arm64); the
script above fetches the right one and puts `furmi` on your `PATH`. It also
installs `furmiculi-update`, so `furmiculi-update` upgrades an
existing install, and `furmi` itself prints a one-line notice when a newer
version is out (set `FURMI_NO_UPDATE_CHECK=1` to turn that off).

Building from source instead:

```sh
cargo install --path .
```

## Usage

```sh
furmi add "Fix the redirect loop after login"   # in the repo you're in; asks if you leave the text out
furmi add --repo ~/Projects/myapp --on-finish pr --in 2h \
       "Fix the redirect loop after login"
echo "Update deps and run tests" | furmi add --repo ~/Projects/myapp --agent codex

furmi start             # the runner and the dashboard together (--no-open: no browser)
furmi stop              # stop the runner (work already underway keeps going in tmux)
furmi agents            # which coding agents are installed, and how to install the rest
furmi agents use codex   # make one the default for new tasks

furmi ls                # queue
furmi ls --schedules    # schedules with their next run
furmi ls --archive      # finished tasks
furmi ls --watch        # ...live, every --interval (default 2s); in a terminal it's
                        # interactive: j/k move, l logs, d/f/c done/fail/cancel (asks
                        # first), r retry, a attach, tab queue/archive, q quit
furmi validate          # check every task and schedule file

furmi run               # run the queue until Ctrl-C (rescans every poll_interval)
furmi run --once        # one pass: enqueue due schedules, reap finished tasks, start new ones
furmi attach [id]       # watch running tasks in tmux, one window each
furmi logs <id>         # print a task's output: live if running, else the saved log
furmi logs <id> --attempt 1   # an earlier attempt, instead of the latest

furmi done [id]         # finish a task: runs on_finish, archives it
furmi fail [id] -r why   # give up on it, without retrying
furmi cancel [id]       # drop a pending or running task
furmi retry <id>        # requeue a failed/cancelled/needs_review task

furmi clean             # remove worktrees of done tasks (-n: dry run)
furmi clean --all --older-than 7d   # ...and of failed/cancelled ones, a week on
furmi clean --archive --all --older-than 30d   # ...and their archived task files/logs

furmi status            # is the runner alive? queue, spend, failing schedules
furmi doctor            # check tmux/git/agents/config/task files; non-zero on problems
furmi service systemd   # print a unit that keeps `furmi run` running (or: launchd)

furmi web               # dashboard at http://127.0.0.1:7433, until Ctrl-C
furmi web --port 8080   # a different port
furmi web --listen 0.0.0.0:7433 --token-file ~/.furmiculi/web-token \
  --allowed-host tools.example   # on a headless box, reached from a tailnet (see below)
```

Inside a task's window, `id` defaults to that task, so an interactive
session ends with a plain `furmi done`, run by you or by the agent.

`furmi run` starts each task in a window of the `furmi` tmux session, inside
`~/.furmiculi/worktrees/<id>` (a git worktree on the task's branch) unless
`worktree: false`. It respects `max_parallel` and `max_parallel_per_repo`,
retries failures after `retry_delay`, kills attempts that exceed `timeout`,
runs `on_finish` on success, and moves finished tasks to `archive/`. Each
attempt's output is saved to `logs/<id>.<attempt>.log`.

## Keeping it running

`furmi run` is a plain foreground process; `furmi service systemd` (Linux) or
`furmi service launchd` (macOS) prints a unit file, with install instructions,
that starts it at login and restarts it if it dies. Only one runner per home
is allowed. `furmi status` shows whether it's alive and when its last pass was
(it warns if the runner looks stuck), and `furmi run` also appends to
`runner.log` in the home folder.

What `furmi` does so that a bad day doesn't lose work:

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
- **Safe git handling.** A reused worktree must belong to the repo, `furmi`
  won't switch your checkout's branch over uncommitted changes, and
  `on_finish: pr` won't open a second PR for a branch that has one.

`furmi web` serves a local dashboard (queue, running, archive and schedules,
refreshed every couple of seconds) — a Sidekiq-style view of the same state
`furmi ls` prints. **+ New task** queues work from a form (what to do, which
project, when to start, what to do with the result), calling the same code as
`furmi add`; `furmi start` is `furmi web` plus a runner that stops with it. If the runner is off,
a banner says so with a **Start the runner** button (that runner keeps going
after you close the dashboard; `furmi stop` or the **stop** link next to
"runner: on" ends it), and **Setup** lists the coding agents (installed or not,
with the install command to copy, and **Use this one** to make one the default)
above what `furmi doctor` finds (tmux, git, task files), with a banner when
something needs attention. furmi doesn't run the installs for you: they're global
`npm install -g` commands and each agent needs you to sign in once.

### On a headless box (your tailnet)

By default `furmi web` and `furmi start` bind to 127.0.0.1 only: the dashboard exists
in your machine's loopback, checked with a loopback `Host` requirement (which
defeats DNS rebinding), a per-run token POSTs must carry, and a loopback
`Origin`. On a server without a screen there's nothing to open 127.0.0.1, so
`--listen` names the address to bind instead (`0.0.0.0:7433` for every
interface). A non-loopback bind refuses to start without two more things:

- `--token-file <path>` (or `--token <value>`, but the file keeps the secret
  out of argv, shell history and `ps`): a shared secret every request must
  carry — `Authorization: Bearer <t>`. Make the file 0600 and put a long
  random string in it, e.g. `openssl rand -hex 16 > ~/.furmiculi/web-token`.
- `--allowed-host <name>` (repeatable): the exact `Host` the browser sends
  when it reaches the dashboard; requests with any other Host are rejected.
  An entry alone allows any port on that host; `host:port` pins the port.

The first page load may pass the token as `?token=<t>`; the server answers
with an `HttpOnly`, `SameSite=Strict` cookie holding it, and the page keeps it
in memory only (never localStorage) and strips it from the URL, then sends it
with every request. The cookie is what makes a reload — or a bookmark — work
after the token is gone from the address bar; without it every refresh would
be a 401. The per-run token and Origin checks POSTs already had stay in place
on top of that, so the cookie alone can't change anything.

On the headless box:

```sh
furmi web --listen 0.0.0.0:7433 --token-file ~/.furmiculi/web-token \
  --allowed-host tools-your-hostname.ts.net --no-open
```

From another machine on the tailnet, open
`http://tools-your-hostname.ts.net:7433/?token=<the file's content>` once;
the dashboard hands that browser a cookie, so from then on
`http://tools-your-hostname.ts.net:7433` works on its own — reloads and
bookmarks included — until the cookie expires (30 days, re-set on each page
load) or you clear it. (`furmi start` takes the same flags, and its runner
stops with the dashboard, as always.) The dashboard speaks plain HTTP: put
`tailscale serve` in front of it if you want HTTPS. And the quietest pattern
is still SSH: `ssh -L 7433:127.0.0.1:7433 your-box`, with `furmi web` keeping
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
as `furmi done|fail|cancel|retry` and `furmi logs`. It only binds to
`127.0.0.1`, so anyone who can reach it could already run `furmi` locally
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
                            # (they run the built `furmi` against real git and tmux)

cargo build                 # browser tests for the dashboard, in e2e/
cd e2e && npm ci && npx playwright install chromium
npm test                    # FURMI_BIN=path/to/furmi to test another build
```

The browser tests start `furmi start`/`furmi web` for each test against a throwaway
home, git repo and tmux server, with a stub agent that runs the prompt as a
shell command, so they need `tmux` and `git` but no real coding agent and touch
none of your files. CI runs both on every push and pull request.
