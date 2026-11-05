# Changelog

Releases and their notes are on the
[releases page](https://github.com/giannileggio/furmiculi/releases). This file
lists what changed between them.

## Unreleased

- **Setup** in the dashboard can now change the runner's limits (tasks at once,
  overall and per repo, and how often it rescans) and the task defaults (mode,
  when-done, worktree, retries, retry delay, timeout). It rewrites those keys in
  `config.toml` and leaves the rest of the file — including your comments —
  alone, and a running `furmi run` picks them up within a poll. A dashboard
  whose session has ended says so instead of quietly showing stale numbers.

- The dashboard's task form can pin a model, like `furmi add --model`: the
  **Model** box is there for a new task, an edit and a schedule, on any machine.
  The suggestions are the ones the agent's own CLI names (`pi --list-models`,
  `opencode models`), asked on the machine that will run the task and kept for
  an hour in `models-cache.json`, plus models your tasks have already run with.
  Agents whose CLI has no such command (claude, codex, gemini, hermes) get a box
  to type into and no list; an agent with no `model_args` can't be handed a
  model, so it gets no box. New optional `models_cmd` per agent in `config.toml`,
  filled in for the pi and opencode presets.

## 0.6.0

- The dashboard has a Stats view: how many tasks finished, the success rate,
  the median run time and the tokens agents reported, tasks finished over time,
  and the same numbers per project, agent and model. It follows the Project,
  Time and search filters, and covers every node.

- Notifications when a task ends, by ntfy and Telegram: `[notify]` in
  `config.toml` picks the endings (`finished`, `failed`, `needs_review`) and the
  services, and nothing is sent unless it's set. Sending runs in a detached
  `curl` with a short timeout, so it can't hold up or fail a task. `furmi doctor`
  checks the config is complete and that `curl` is installed.

- The runner now tidies up after itself, at start and then hourly: it removes
  the clean worktrees of done tasks, trims logs past the last 5 attempts, and
  requeues tasks that failed because of the machine (tmux gone, agent not
  installed) once that's fixed, at most twice. The dashboard shows a banner with
  `furmi doctor` problems, anything it declined to do, and merged `furmi/*`
  branches with the command to delete them, which it never does itself. Pausing
  the runner pauses the changes too. Configure or turn it off under
  `[housekeeping]`.

- `furmi restart` stops the dashboard and the runner and starts them again the
  way they were started, so they run the installed binary. Running tasks keep
  going in tmux, and under `furmi service` systemd or launchd does the restart.
  The start flags are kept in the dashboard's lock file, never the token: a
  dashboard started with `--token` is refused and left running.

- `furmi update` now ends by naming a dashboard or runner still on the old
  version instead of leaving them to be found out, and never restarts them
  unasked; `furmi update --restart` does.

- The running version is visible: in the dashboard header, beside other
  machines in its sidebar, and in `furmi status` and `furmi node ls`, with
  anything older than the installed binary flagged. A page left open across an
  upgrade offers a reload.

- Tasks can run a plain shell command instead of an agent: `furmi add --run
  "<command>"`, or `run:` in a task or schedule file. It goes through `sh -c` with
  no prompt, agent or model and shares the logs, `timeout`, `retries` and
  `on_finish` of any other run. It runs in the repo folder unless you add
  `--worktree`, and `[defaults]` don't make it commit. `furmi schedule` takes
  `--run` too, `furmi validate` rejects one that also sets an agent, model, mode
  or prompt, and `furmi ls` and the dashboard show it as `command`. `furmi add` and
  `furmi schedule` also gain `--worktree`, the opposite of `--no-worktree`.

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

- A remote dashboard no longer keeps the token in a 30-day cookie. Opening
  `?token=` now redirects to the plain address and sets a 15-minute `HttpOnly`
  session cookie (`Secure` behind an `X-Forwarded-Proto: https` proxy) that
  slides while the page is open and ends on restart, so a rotated token or a
  stolen cookie doesn't outlive its welcome.

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
