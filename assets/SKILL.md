---
name: furmi-tasks
description: Create, schedule, edit or cancel furmiculi (furmi) tasks, the Markdown files that queue work for coding agents to run later. Use when the user asks to add, queue or schedule a task (for this repo or another), to set up something recurring ("every night...", "each Monday..."), or to change or drop queued work.
---

# Managing furmi tasks

If `furmi` isn't on the PATH, install it first (Linux or macOS; needs git
and tmux), then run `furmi init` to set up `{home}`:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/giannileggio/furmiculi/main/install.sh | sh
```

The queue lives in `{home}`. The full format is in `{home}/FORMAT.md`; read
it if anything here is unclear. Run every command exactly as written here
(`{furmi} ...`), from any folder.

A task is run later, by another agent, in the target repo. That agent has no
access to this conversation. Your job is to turn what the user said into a
task it can carry out.

## 1. Work out the details

- **Repo**: an absolute path or one starting with `~`. "This repo", or no
  repo at all while you're working in a git repo outside `{home}`, means that
  repo's root (`git rev-parse --show-toplevel`). If the user gives only a
  name, look for it in the `REPO` column of `{furmi} ls`, `{furmi} ls --archive` and
  `{furmi} ls --schedules`, or check that `~/Projects/<name>` exists. If it's
  still ambiguous, ask. Don't guess.
- **What to do**: if the request is vague ("clean up the auth code"), ask
  one or two questions before you queue it. A vague task wastes a whole run.
- **When**: now (the default), at a given time, after a delay, or on a
  recurring schedule (see section 4).
- **Only set fields the user asked for.** Anything you leave out falls back
  to `[defaults]` in `{home}/config.toml`, which is what the user expects.
  Examples: "open a PR" → `--on-finish pr`; "I'll drive it" →
  `--mode interactive` (they follow it with `{furmi} attach <id>`); "use
  codex" → `--agent codex` (the allowed names are the `[agents.*]` sections
  in config.toml, and `{furmi} agents` shows which are installed — a task
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

## 3. Create the task with `{furmi} add`

Always use `{furmi} add`, never a hand-written file in `{home}/tasks/`. It
validates the task before creating it, picks a unique id, and stores
`created_at`, so `furmi run` never sees a half-written task.

```sh
{furmi} add --repo ~/Projects/myapp --created-by agent --on-finish pr <<'EOF'
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
| `--id <id>` | File name and id: `a-z`, `0-9`, `-`. Default: derived from the prompt's first line. Pass a short descriptive one. |
| `--branch <name>` | Default `furmi/<id>`. |
| `--no-worktree` | Work directly in the repo checkout. Only when the user asks: the agent edits the checkout they may be working in, and `on_finish` commits everything there. |
| `--agent <name>`, `--model <id>` | See step 1. |
| `--mode headless\|interactive` | |
| `--on-finish none\|commit\|push\|pr` | |
| `--retries <n>`, `--retry-delay 5m`, `--timeout 2h` | |
| `--at <RFC 3339>` | e.g. `2026-09-28T02:00:00+02:00`. Use the local UTC offset (`date +%:z`). |
| `--in <duration>` | e.g. `2h`. |

On success it prints the new file's path (a person at a terminal sees a short
summary instead). On failure it lists the problems; fix them and run it
again.

## 4. Recurring work: schedules

For "every night", "each Monday" and the like, write
`{home}/schedules/<id>.md` yourself. Use `id` plus the fields from step 1
(`repo`, `branch`, `agent`, `model`, ...), plus a quoted 5-field `cron` in
local time. Schedules have no `created_by`/`created_at`/`scheduled_at`: each
firing sets those on the task it creates. The body is the prompt, written as
in step 2.

```markdown
---
id: weekly-deps
cron: "0 6 * * 1"
repo: ~/Projects/myapp
on_finish: pr
---

Update dependencies to their latest compatible versions...
```

Then run `{furmi} validate {home}/schedules/<id>.md` and fix anything it
reports. Use an id that doesn't already exist in `{home}/schedules/`. The
schedule first fires at the next matching time after the runner sees it
(`{furmi} run` or `{furmi} start`). A firing is skipped while the previous
task from the same schedule is still queued or running; add
`allow_overlap: true` only if the user wants runs to pile up.

## 5. Changing or dropping queued work

- Check the current state with `{furmi} ls` (or read the file) first.
- **Pending tasks** and **schedules**: you may edit the prompt and the fields
  from step 1, then run `{furmi} validate <file>`. To pause a schedule, set
  `enabled: false`.
- Change a status only with these commands, and confirm with the user first:
  `{furmi} cancel <id>` (pending or running), `{furmi} retry <id>` (failed,
  cancelled or needs_review: requeues it), `{furmi} done <id>` (running,
  needs_review or failed: runs `on_finish`, e.g. opens the PR) and
  `{furmi} fail <id> -r "<why>"` (running or needs_review). The `error:` field
  says why a task failed; `{furmi} logs <id>` prints its output.
- Never edit fields furmi writes (`status`, `attempts`, `started_at`,
  `finished_at`, `exit_code`, `tmux_window`, `error`, `interruptions`,
  `tokens_in`, `tokens_out`, `model_used`, and a schedule's
  `last_enqueued_at`), and never delete task files.
- Don't edit `running` tasks, or anything in `{home}/archive/`, `logs/` or
  `worktrees/`.

## 6. Report back

Tell the user the task id, when it will start, and what happens when it
finishes (e.g. "opens a PR on `furmi/fix-login-redirect`"). Tasks only run
while the runner is up; `{furmi} status` says whether it is, so check it
before promising a start time. If the runner is down,
say that `{furmi} start` starts it and serves the dashboard (`{furmi} start -d`
leaves both running after the shell closes), and that `{furmi} doctor` says
what's missing when it won't run.
