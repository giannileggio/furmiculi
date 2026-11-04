# Changelog

Releases and their notes are on the
[releases page](https://github.com/giannileggio/furmiculi/releases). This file
lists what changed between them.

## Unreleased

- `furmi schedule` manages schedules from the terminal: `ls`, `add` (the
  options of `furmi add`, plus `--cron` and `--allow-overlap`), `edit <id>`,
  `pause <id>`, `resume <id>` and `delete <id>`. It writes the files the
  dashboard does, and the furmi-tasks skill now uses it instead of hand-written
  schedule files.

- `furmi pause` holds the runner without stopping it: nothing new starts and no
  schedule fires, while running tasks carry on and are still finished.
  `furmi resume` undoes it. It lasts across runner restarts, shows in
  `furmi status`, and the dashboard header has a toggle.

- A `hermes` agent preset (`hermes -z {prompt}` headless), alongside the other
  built-in agents. It ships without an auto-approve flag, like the rest.

- `furmi clean` no longer removes a worktree it can't inspect. A worktree whose
  owning repo moved or was deleted fails `git status`, and that used to count as
  "no changes": the worktree was then removed as broken, taking any uncommitted
  work in it with it. It is now reported (`kept <id>: can't inspect: …`) and
  kept, and `furmi clean --remove-broken` removes those worktrees instead. The
  dry run and the real run agree.

## 0.5.1

- `furmi node add <name> <ssh-target> --furmi '<command>'` says how to start
  furmi on a node that isn't plain `furmi` on the PATH, e.g.
  `--furmi 'docker exec -i pod furmi'` for one inside a container that the SSH
  login only reaches the host of.

- The dashboard has permalinks. An open task is `/tasks/<id>`, an open log
  `/logs/<id>` and a list `/views/<id>`, with `?project=`, `?age=` and `?q=` for
  the narrowing. Copy the address to share it; Back closes a task or log.
- The search box moved from the header into the toolbar, next to a new Project
  filter. It says which list it searches, shows "3 of 12" while it narrows, and
  on Repeating it matches what the row shows (Active, Paused, "Every day at 9:00").
- `furmi web --token <value>` is refused together with `-d`/`--detach`: the
  backgrounded dashboard's command line would carry the secret in `ps` and
  /proc for as long as it serves. Use `--token-file` (the setup answer already
  does), or run without `--detach`.

## 0.5.0

- Nodes are reached over SSH instead of HTTP and a token:
  `furmi node add box me@box`. The hub runs `furmi node call` on the other
  machine, so that machine needs no open port, no running dashboard and no
  token, and OpenSSH does the authenticating and encrypting. `node add` checks
  the connection first and saves nothing if it fails. Every node can be changed
  now (there are no read-only nodes), and `nodes.toml` holds no secrets. A node
  added the old way (`url` and `token`) has to be added again, and both machines
  need this release.

## 0.4.0

- The dashboard no longer polls. It opens a Server-Sent Events stream
  (`GET /api/events`) and redraws when the queue changes, and the log view
  follows a running task over `GET /api/tasks/<id>/logs/stream`. Restart a
  running `furmi start`/`furmi web` after upgrading: an older one doesn't have
  the streams, so the page would never update.
- `furmi node add/ls/rm` adds other machines running `furmi web`; the dashboard
  then shows their tasks and schedules next to yours with a Node column, and
  names any that don't answer. Their buttons work from here, and **+ New task**
  can send a task to another machine, which finds its own clone of the repo by
  git remote. Upgrade the other machines too: older ones can be read but not
  given tasks.
- `furmi add --scratch` (and **No project** in **+ New task**) queues a task
  with no repo: it runs in an empty folder of its own, `scratch/<id>`, which
  stays until `furmi clean`. It can go to any machine, since there is no clone to
  match.
- **+ New task** can pick **Any machine (least busy)**: the dashboard sends the
  task to an online machine that has a clone of the repo, preferring a free
  slot, then the fewest running tasks. A repeating schedule can be created on
  another machine (or on one picked when it's created) the same way.
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
