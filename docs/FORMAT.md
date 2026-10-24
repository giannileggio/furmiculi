# File formats

All state lives under one folder: `$FURMI_HOME`, default `~/.furmiculi`.

```
~/.furmiculi/
  config.toml    user preferences and defaults
  tasks/         queue: one <id>.md per task
  schedules/     recurring task templates: one <id>.md per schedule
  archive/       finished tasks, moved out of tasks/
  logs/          <id>.<attempt>.log (the agent's output), .exit (exit status)
                 and .usage (token/cost usage, if the agent reports it) per run
  worktrees/     <id>/: the git worktree a task runs in (kept until `furmi clean`)
  runner.log     what `furmi run` did, appended (moved to runner.log.1 past 5 MB)
  runner.json    `furmi run`'s heartbeat: pid, start and last pass (see `furmi status`)
  AGENTS.md      guide for any agent opened in this folder
  CLAUDE.md, GEMINI.md   `@AGENTS.md`, for agents that don't read AGENTS.md
  FORMAT.md      this document
  .agents/skills/furmi-tasks/SKILL.md   how an agent creates and edits tasks
  .claude/skills/furmi-tasks            link to it, for agents with their own skill folder
```

Open any coding agent in this folder and ask it to "add a task to..." or
"every night, ...": the `furmi-tasks` skill tells it how. `furmi init` keeps your
`AGENTS.md`, `CLAUDE.md` and `GEMINI.md` if they exist, and rewrites
`FORMAT.md` and the skill so they match the installed `furmi`.

`furmi init` also offers to make the skill available in every project (or pass
`--global-skill` / `--no-global-skill`): it links `~/.agents/skills/furmi-tasks`,
and `~/.claude/skills/furmi-tasks` if `~/.claude` exists, to this folder's skill.
Agents can then queue tasks from inside any repo ("queue a task for this
repo..."). Once linked, every `furmi init` keeps the links pointing here.

Tasks and schedules are Markdown files with a YAML frontmatter block. The
**body is the prompt** given to the agent. Unknown frontmatter keys are
errors, so typos don't silently fall back to defaults. Run `furmi validate` after
editing any file.

## Task — `tasks/<id>.md`

```markdown
---
id: fix-login-redirect
repo: ~/Projects/myapp
branch: fix/login-redirect
on_finish: pr
retries: 3
scheduled_at: 2026-09-28T02:00:00+02:00
---

Fix the redirect loop after login when the session cookie has expired.
Add a regression test.
```

### Fields you write

| Field          | Required | Default (config `[defaults]`) | Meaning |
|----------------|----------|-------------------------------|---------|
| `id`           | yes      | —                 | `a-z`, `0-9`, `-`, max 64 chars. Must equal the file name without `.md`. |
| `repo`         | yes      | —                 | Folder the task runs in: absolute, or starting with `~`. |
| `branch`       | no       | `furmi/<id>` if `worktree` | Branch to work on. |
| `worktree`     | no       | `true`            | Run in a dedicated git worktree for the branch. |
| `agent`        | no       | chosen at `furmi init` | Name of an agent in `config.toml` `[agents.*]`. |
| `model`        | no       | the agent's own   | Model id, in the form that agent's CLI expects. |
| `mode`         | no       | `headless`        | `headless`: non-interactive, exit code decides the outcome. `interactive`: a live session that finishes with `furmi done` / `furmi fail`. |
| `on_finish`    | no       | `none`            | `none` \| `commit` \| `push` \| `pr` |
| `retries`      | no       | `1`               | Extra attempts after a failure. |
| `retry_delay`  | no       | `5m`              | Wait between attempts (`30s`, `5m`, `1h`...). |
| `timeout`      | no       | `2h`              | Per attempt. |
| `scheduled_at` | no       | now               | Don't start before this RFC 3339 time. |
| `created_by`   | no       | —                 | Free text: `cli`, `agent`, `schedule:<id>`... |
| `created_at`   | no       | —                 | RFC 3339; set by `furmi add`. |

### Fields furmiculi writes

Don't edit these by hand. Use `furmi` commands to change the status.

| Field         | Meaning |
|---------------|---------|
| `status`      | `pending` → `running` → `done` \| `failed` \| `cancelled` \| `needs_review` |
| `attempts`    | Attempts started so far. |
| `interruptions` | Attempts cut short from outside (tmux or the machine went away). They are requeued at once and don't count against `retries`, up to 5 times. |
| `started_at`, `finished_at` | RFC 3339. |
| `exit_code`   | Headless mode only. |
| `tmux_window` | Where the task is or was running. |
| `error`       | Last failure reason. |
| `tokens_in`, `tokens_out` | Token usage summed over attempts, if the agent's headless mode reports it (built in for `claude`). |
| `cost_usd`    | Cost in USD, same condition. |

`needs_review` means an interactive session ended without signalling
`furmi done` or `furmi fail`.

### Changing the status by hand

| Command | Works on | Effect |
|---------|----------|--------|
| `furmi done [id]`   | running, needs_review, failed | Ends the session if running, runs `on_finish`, archives as `done` (or `failed` if `on_finish` fails). |
| `furmi fail [id] [-r reason]` | running, needs_review | Ends the session if running, archives as `failed` without retrying. |
| `furmi cancel [id]` | pending, running | Ends the session if running, archives as `cancelled`. `on_finish` doesn't run. |
| `furmi retry <id>`  | failed, cancelled, needs_review | Moves it back to `tasks/` as `pending`, to start as soon as possible in the same worktree. |
| `furmi attach [id]` | running | Opens the task's tmux window, or the whole session without an id. |

`id` defaults to `$FURMI_TASK_ID`, so inside a task's window a plain `furmi done`
works: run it in the agent's shell, or ask the agent to run it.
Ending a session saves its output to the attempt's log first.

`furmi logs <id> [--attempt N]` prints that output: the live tmux pane for a
task still running, else `logs/<id>.<attempt>.log` (the latest attempt by
default). A headless agent's stdout and stderr are streamed to that file as
they arrive (up to 64 MB per attempt), so the whole log survives tmux dying.
Interactive sessions' logs are captured from the tmux pane when they end.

`furmi clean` removes `worktrees/<id>` of archived `done` tasks (`--all`: any
archived task, and worktrees with no task file; `--older-than 7d`: only
tasks finished that long ago; `-n`: dry run). It never removes a worktree
with uncommitted changes, and never deletes branches.

`furmi clean --archive` also deletes the archived task file and its
`logs/<id>.*` files, for any task whose worktree is already gone (or never
had one). This loses that task's history from `furmi ls --archive`/`furmi web`
for good, so it's off by default; combine it with `--all --older-than` to
prune old history without touching anything recent.

`attempts` keeps counting across `furmi retry`, so earlier logs are kept. That
also means automatic retries aren't renewed: if the retried attempt fails,
the task is `failed` unless `retries` still covers the new count.

A failed attempt (non-zero exit, signal, timeout, or a start error such as a
worktree that can't be created) goes back to `pending` with `scheduled_at`
pushed out by `retry_delay` while retries remain; then it's `failed`. A task
that doesn't pass `furmi validate` when its turn comes is failed without
running. `timeout` applies to headless tasks only (an interactive session
waits for a human); when it hits, the agent's process group gets SIGTERM and,
5 seconds later, SIGKILL. If `on_finish` fails after the agent succeeded, the
task is `failed` without a retry; its worktree keeps the work. `on_finish:
pr` doesn't open a second PR if the branch already has one, so `furmi done` can
be re-run after a partial failure.

A running task whose tmux window is gone without an exit record (a reboot,
a killed tmux server) is *interrupted*, not failed: it goes back to
`pending` at once and starts again, in the same worktree, without using up a
retry (see `interruptions`).

`prompt` is passed to the agent as a single command-line argument, so it
must stay under 100,000 bytes (`furmi validate` and `furmi add` check this): put
long specifications in a file in the repo and point the prompt at it.

With `worktree: true`, an existing `worktrees/<id>` is reused only if it's
really a worktree of the task's repo. With `worktree: false`, `furmi` refuses to
switch the repo's branch while it has uncommitted changes.

The agent runs with `FURMI_HOME` and `FURMI_TASK_ID` set.

## Schedule — `schedules/<id>.md`

A schedule has `id` plus the same `repo`/`branch`/`worktree`/`agent`/`model`/
`mode`/`on_finish`/`retries`/`retry_delay`/`timeout` fields as a task. It has
no `scheduled_at`, `created_by` or `created_at`: each firing sets those on
the task it creates. It also adds:

| Field              | Required | Meaning |
|--------------------|----------|---------|
| `cron`             | yes      | 5-field cron expression in local time, e.g. `"0 2 * * *"`. Quote it. |
| `enabled`          | no       | Default `true`. |
| `allow_overlap`    | no       | Default `false`: a firing is skipped while the previous task from this schedule is still in `tasks/` (pending or running), so a slow task isn't joined by a pile of copies. |
| `last_enqueued_at` | —        | Written by furmiculi. |

```markdown
---
id: nightly-deps
cron: "0 2 * * *"
repo: ~/Projects/myapp
on_finish: pr
---

Update dependencies, run the test suite, and fix any breakage.
```

Each time the schedule fires, `furmi run` creates a task `<id>-<YYYYMMDD-HHMM>`
in `tasks/` with the schedule's fields and prompt and sets
`created_by: schedule:<id>`. The first time `furmi run` sees a schedule it only
records `last_enqueued_at`; it fires from then on. Firings missed while
`furmi run` wasn't running collapse into one task, not one per missed slot.
`furmi run` logs a warning when a schedule's last 3 runs all failed, and
`furmi status` lists schedules whose last 2 did.

## Config — `config.toml`

`furmi init` writes a `config.toml` with every default, commented, and asks
which agent tasks use by default (or detects the only one on `$PATH`, or
takes `--agent`). Presets are built in for `claude`, `codex`, `gemini`,
`opencode` and `pi`; add an `[agents.<name>]` table for any other CLI agent:

```toml
[agents.aider]
headless = ["aider", "--yes-always", "--message", "{prompt}"]
interactive = ["aider", "--message", "{prompt}"]
model_args = ["--model", "{model}"]   # appended only when a model is set
model = "sonnet"                      # optional: this agent's default model
```

`headless` and `interactive` are argv lists with `{prompt}` and `{id}`
placeholders. No shell is involved, so prompts need no quoting. A task's
`model` (else the agent's `model`) fills `{model}` in `model_args`; with
neither, nothing is added and the agent picks its own model.

### Runner limits

```toml
[runner]
daily_budget_usd = 20.0   # optional
```

Once the cost agents reported for tasks run today (local time) reaches
`daily_budget_usd`, `furmi run` starts no more tasks until tomorrow; running
ones finish. Only agents that report a cost count (see below). `furmi status`
shows the day's spend.

### The dashboard

```toml
[web]
configured = true                    # the setup questions have been answered
listen = "0.0.0.0:7433"              # optional: default 127.0.0.1:7433
allowed_hosts = ["box.tailnet.ts.net"]  # the Host headers browsers send; ["*"] = any
token_file = "~/.furmiculi/web-token"   # 0600, shared secret
```

Where `furmi web` and `furmi start` serve from, and how they guard it. Nothing
here means loopback only, which is the default and needs no secret. A
non-loopback `listen` refuses to start with an empty `allowed_hosts` or no
`token_file`, and every request then needs `Authorization: Bearer` with that
file's contents (the first page load may pass it as `?token=` instead, and
gets a cookie for later ones). The first interactive `furmi web` asks the
questions and writes this table — including `configured = true` on its own
when the answer is "this machine only", so it never asks twice; `furmi web
--setup` asks again. Flags override the table per setting. A table without
`configured` (hand-written, say) is never asked about.

### Token/cost usage

`furmi` records what a headless run's stdout reports as `tokens_in`,
`tokens_out` and `cost_usd` on the task (`furmi ls` shows them; the dashboard
doesn't, since a column that is mostly dashes reads like accounting). Two
shapes are understood:

- one JSON object with a `usage` field — what `claude --output-format json`
  prints at the end of a run;
- an event stream, one JSON object per line, whose finished assistant
  messages each report their own usage — what `pi --mode json` prints. Those
  turns are summed into one total, and an `error`/`aborted` stop reason in
  there counts as a failed run: that mode exits 0 either way, so the stream
  is the only place a failure is reported.

The `claude` and `pi` presets ask for those flags. Other agents report
nothing unless their own headless flags produce one of the two shapes.
Interactive mode never reports usage: it keeps a real terminal for the
human running it.
