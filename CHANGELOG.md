# Changelog

Releases and their notes are on the
[releases page](https://github.com/giannileggio/furmiculi/releases). This file
lists what changed between them.

## Unreleased

- `furmi node add/ls/rm` adds other machines running `furmi web`; the dashboard
  then shows their tasks and schedules next to yours with a Node column, and
  names any that don't answer. Their buttons work from here, and **+ New task**
  can send a task to another machine, which finds its own clone of the repo by
  git remote. Upgrade the other machines too: older ones can be read but not
  given tasks.
- **+ New task** can pick **Any machine (least busy)**: the dashboard sends the
  task to an online machine that has a clone of the repo, preferring a free
  slot, then the fewest running tasks.
- New tasks record `remote:`, the repo's `origin` as `host/owner/name`, and the
  dashboard serves `GET /api/repos` (the repos and agents a machine can host,
  by remote). First step towards running tasks across machines. Restart a
  running `furmi start`/`furmi web` after upgrading: an older one rejects the
  new key and skips those tasks.

## 0.3.0

- The dashboard has a new layout: a sticky header with a runner pill and search,
  and a sidebar of lists (Waiting, Working, Needs review, Failed, Finished,
  Repeating) and projects, whose numbers follow the project you pick. A task
  opens in a drawer from the right, the tables fit the screen at any width, and
  the logo links home. Tick several tasks (the Select button or `x`) to mark
  them done or failed, drop them or try them again together (#32).
- The install script now ends by asking whether to make the `furmi-tasks` skill
  available in every project, so agents can queue tasks from any repo. Set
  `FURMI_NO_SKILL=1` to skip the question.
- Cost is no longer recorded or limited: `cost_usd` and `[runner]
  daily_budget_usd` are gone (an agent's printed cost is an API-equivalent
  estimate, not what a subscription charges). Remove `daily_budget_usd` from
  config.toml, which now rejects it. Tokens stay, and `furmi status` totals
  the day's.
- A dashboard served with `--allowed-host '*'` now prints a warning at start-up:
  the bearer token is then the only gate.
- The remote-access documentation moved to [docs/REMOTE.md](docs/REMOTE.md).
- `on_finish: pr` no longer fails a task that changed nothing; the per-repo
  parallelism limit keys on repository identity; a task run directly in a repo
  checkout (`worktree: false`) is now loud about it (#24, #25, #26).

## 0.2.0

- `furmi update` installs a release over the running binary (#22).
- The dashboard is a list you can work through, with keyboard shortcuts, and
  the model a run used is recorded (#20).
- The repository was prepared to be public.

## 0.1.0

First release.
