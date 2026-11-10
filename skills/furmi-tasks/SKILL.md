---
name: furmi-tasks
description: Create, schedule, edit or cancel furmiculi (furmi) tasks, the Markdown files that queue work for coding agents to run later. Use when the user asks to add, queue or schedule a task (for this repo or another), to set up something recurring ("every night...", "each Monday..."), or to change or drop queued work.
---

# Managing furmi tasks

If `furmi` isn't on the PATH, install it first (Linux or macOS; needs git
and tmux), then run `furmi init` to set up `~/.furmiculi`:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/giannileggio/furmiculi/main/install.sh | sh
```

The queue lives in `~/.furmiculi`. The full format is in `~/.furmiculi/FORMAT.md`; read
it if anything here is unclear. Run every command exactly as written here
(`furmi ...`), from any folder.

A task is run later, by another agent, in the target repo. That agent has no
access to this conversation. Your job is to turn what the user said into a
task it can carry out.

## 1. Work out the details

- **Repo**: an absolute path or one starting with `~`. "This repo", or no
  repo at all while you're working in a git repo outside `~/.furmiculi`, means that
  repo's root (`git rev-parse --show-toplevel`). If the user gives only a
  name, look for it in the `REPO` column of `furmi ls`, `furmi ls --archive` and
  `furmi ls --schedules`, or check that `~/Projects/<name>` exists. If it's
  still ambiguous, ask. Don't guess.
- **What to do**: if the request is vague ("clean up the auth code"), ask
  one or two questions before you queue it. A vague task wastes a whole run.
- **When**: now (the default), at a given time, after a delay, or on a
  recurring schedule (see section 4).
- **Queue it, or save a draft?** A queued task starts as soon as the runner
  sees it, and the user can't look it over first. A **draft** is saved but
  doesn't run until the user queues it (in the dashboard, or with
  `furmi queue <id>`). This is the user's call, not yours: unless they have
  already said ("queue it", "let me review it first") or told you to always do
  one or the other, ask once, before you create anything. Don't choose for them.
  Recurring schedules are created as asked; this is about one-off tasks.
- **Only set fields the user asked for.** Anything you leave out falls back
  to `[defaults]` in `~/.furmiculi/config.toml`, which is what the user expects.
  Examples: "open a PR" → `--on-finish pr`; "I'll drive it" →
  `--mode interactive` (they follow it with `furmi attach <id>`); "use
  codex" → `--agent codex` (the allowed names are the `[agents.*]` sections
  in config.toml, and `furmi agents` shows which are installed — a task
  whose agent isn't installed fails); a named model → `--model` with the
  model id in the form that agent's CLI expects.

## 2. Write the prompt

The prompt is the task body. Write it for an engineer who is new to the repo
and can't ask questions (headless runs have nobody to answer):

- **Goal**: the outcome, in one or two sentences.
- **Context**: everything the user gave you, such as error messages, file
  names, issue links and reproduction steps. Copy them, don't summarize.
- **Constraints**: anything the user wants done, or left alone.
- **Done when**: how to check the work, e.g. which tests or commands must
  pass, and "add a regression test" if it's a bug fix.
- **If blocked**: tell the agent to stop and explain what's missing instead
  of guessing.

Don't invent requirements the user didn't ask for. Don't mention furmi; the
runner handles branches, worktrees, commits and PRs as configured.

## 3. Create the task with `furmi add`

Always use `furmi add`, never a hand-written file in `~/.furmiculi/tasks/`. It
validates the task before creating it, picks a unique id, and stores
`created_at`, so `furmi run` never sees a half-written task.

```sh
furmi add --repo ~/Projects/myapp --created-by agent --on-finish pr <<'EOF'
Fix the redirect loop after login when the session cookie has expired.

Reproduce: log in, delete the `session` cookie, reload /dashboard. The
browser loops between /login and /dashboard.

Done when: the expired-session case redirects to /login once, and a
regression test covers it. `npm test` passes.
EOF
```

Options (pass `--repo` unless the task is for the repo you're in; the rest
fall back to `[defaults]`):

| Option | Meaning |
|--------|---------|
| `--repo <path>` | The project to run in. Defaults to the git repo you're in. |
| `--scratch` | No project: run in a new empty folder of its own (`~/.furmiculi/scratch/<id>`, kept until `furmi clean`), for research or drafts. Nothing is committed. Not with `--repo`, `--branch`, `--worktree`, `--no-worktree` or `--on-finish`. |
| `--id <id>` | File name and id: `a-z`, `0-9`, `-`. Default: derived from the prompt's first line. Pass a short descriptive one. |
| `--branch <name>` | Default `furmi/<id>`. |
| `--worktree`, `--no-worktree` | Run in a dedicated git worktree, or directly in the repo checkout. The second only when the user asks: the agent edits the checkout they may be working in, and `on_finish` commits everything there. |
| `--agent <name>`, `--model <id>` | See step 1. |
| `--mode headless\|interactive` | |
| `--on-finish none\|commit\|push\|pr` | |
| `--retries <n>`, `--retry-delay 5m`, `--timeout 2h` | |
| `--at <RFC 3339>` | e.g. `2026-09-28T02:00:00+02:00`. Use the local UTC offset (`date +%:z`). |
| `--in <duration>` | e.g. `2h`. |
| `--draft` | Save it as a draft instead of queueing it (see step 1). Add it only when the user chose a draft. |
| `--template <id>` | Start from a template (section 5): its prompt and options are used, and anything you pass replaces them. |

On success it prints the new file's path (a person at a terminal sees a short
summary instead). On failure it lists the problems; fix them and run it
again.

### Commands, not agents

When the work is a known shell command, with nothing to work out ("run the
backup script tonight", "run `make release-notes` every Friday"), queue the
command itself instead of asking an agent to run it. No prompt and no agent:

```sh
furmi add --repo ~/Projects/myapp --created-by agent --run "./scripts/backup.sh" --in 2h
```

`--run <command>` goes through `sh -c` in the repo folder itself, with stdin
closed, and exit status 0 means success. It can't be combined with a prompt,
`--agent`, `--model` or `--mode`. `--timeout`, `--retries`, `--retry-delay`, `--at`
and `--in` work as usual, but a retry runs the whole command again: pass
`--retries 0` unless it's safe to repeat. Add `--worktree` for a checkout of its
own, and `--on-finish` only if it's asked for: unlike an agent task, `[defaults]`
don't choose either for a command, so by default nothing is committed.

## 4. Recurring work: schedules

For "every night", "each Monday" and the like, create a schedule with
`furmi schedule add`. It takes the same options as `furmi add` (`--repo`,
`--branch`, `--worktree`, `--no-worktree`, `--agent`, `--model`, `--mode`,
`--on-finish`, `--retries`, `--retry-delay`, `--timeout`, `--id`, `--prompt` or
the prompt on stdin) plus a required `--cron`, a 5-field expression in local
time, and `--allow-overlap`. A schedule has no `--created-by`, `--at`, `--in`,
`--scratch`, `--draft` or `--template`: each firing sets `created_at` and
`created_by: schedule:<id>` on the task it creates. The prompt is
written as in step 2. `--run <command>` works here too, in place of the prompt.

```sh
furmi schedule add --repo ~/Projects/myapp --cron "0 6 * * 1" --on-finish pr <<'EOF'
Update dependencies to their latest compatible versions...
EOF
```

It validates like `furmi validate`, refuses an id that is taken, and prints the
path of the new file. The schedule first fires at the next matching time after
the runner sees it (`furmi run` or `furmi start`). A firing is skipped while the
previous task from the same schedule is still queued or running; add
`--allow-overlap` only if the user wants runs to pile up. Don't write files in
`~/.furmiculi/schedules/` by hand.

## 5. Drafts and templates

- `furmi queue <id>` queues a draft (with `--at` or `--in` for a later start).
  Run it only when the user says to. `furmi cancel <id>` on a draft discards it.
- `furmi duplicate <id>` copies any task, waiting or finished, into a new
  draft ("do that again", "same thing for another repo": then change the draft's
  file, or ask the user to). `--as <new-id>` names the copy.
- A **template** is a task kept to start new ones from. `furmi template ls`
  lists them; `furmi add --template <id> --repo ~/Projects/myapp` creates a
  task from one. When the user asks for work a template already covers, use it
  instead of writing the prompt again, and add what is specific to this request
  as extra detail in the prompt. `furmi template save <task-id>` turns an
  existing task into a template. Create and change templates only when the user
  asks; don't write files in `~/.furmiculi/templates/` by hand.

## 6. Changing or dropping queued work

- Check the current state with `furmi ls` (or read the file) first.
- **Pending and draft tasks**: edit the file and run
  `furmi validate <file>`. For a schedule use `furmi schedule edit <id>` with
  just the options to change (`--cron`, `--prompt`, `--run`, ...), `furmi schedule pause <id>`,
  `resume <id>` and `delete <id>`; `furmi schedule ls` lists them.
- Change a status only with these commands, and confirm with the user first:
  `furmi cancel <id>` (draft, pending or running; a draft is deleted),
  `furmi retry <id>` (failed,
  cancelled or needs_review: requeues it), `furmi done <id>` (running,
  needs_review or failed: runs `on_finish`, e.g. opens the PR) and
  `furmi fail <id> -r "<why>"` (running or needs_review). The `error:` field
  says why a task failed; `furmi logs <id>` prints its output.
- Never edit fields furmi writes (`status`, `attempts`, `started_at`,
  `finished_at`, `exit_code`, `tmux_window`, `error`, `interruptions`,
  `auto_requeues`, `created_at`, `remote`, `tokens_in`, `tokens_out`,
  `model_used`, and a schedule's `last_enqueued_at`), and never delete task
  files.
- Don't edit `running` tasks, or anything in `~/.furmiculi/archive/`, `logs/` or
  `worktrees/` or `scratch/`.

## 7. Report back

For a draft, give the id and say it will not run until it is queued: they can
open it in the dashboard, edit it and press Queue, or you can run
`furmi queue <id>` once they say so. Otherwise tell the user the task id, when
it will start, and what happens when it
finishes (e.g. "opens a PR on `furmi/fix-login-redirect`"). Tasks only run
while the runner is up and not paused; `furmi status` says which, so check it
before promising a start time. If the runner is down,
say that `furmi start` starts it and serves the dashboard (`furmi start -d`
leaves both running after the shell closes), that `furmi resume` lifts a pause,
and that `furmi doctor` says what's missing when it won't run.
