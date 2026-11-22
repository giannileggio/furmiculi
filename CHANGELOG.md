# Changelog

Releases and their notes are on the
[releases page](https://github.com/giannileggio/furmiculi/releases). This file
lists what changed between them.

## Unreleased

Command line

- `furmi update` also rewrites the files furmi owns in the home — `FORMAT.md`
  and the `furmi-tasks` skill — with the release it just installed, so an
  agent's copy of the instructions catches up with the binary instead of
  describing the one it replaced. It runs through the new binary (the one doing
  the update still carries the old copies), the global
  `~/.agents/skills/furmi-tasks` link follows on its own, and a home that never
  had these files is left alone. A refresh that fails says so without failing
  the update.

## 0.9.0

Dependencies

- Durations (`5m` in config.toml, `1h 30m` in a message, `--retry-delay 30s`)
  are parsed and written by furmi itself now, in `src/duration.rs`, instead of
  the `humantime` and `humantime-serde` crates, which went unmaintained when
  chrono did (RUSTSEC-2026-0542). Existing config.toml files and the text
  furmi prints are unchanged. chrono itself stays, with RUSTSEC-2026-0543
  ignored in `deny.toml` until it is migrated to jiff or time.

Security

- A dashboard on this machine now asks each browser to sign in once. Its page
  carries the token that lets it queue a task, and any program on the machine,
  of any user, could fetch that page from 127.0.0.1 and queue work that runs as
  you. The secret is a file only you can read (`.local-token`); `furmi start`
  opens the browser signed in, `furmi open` does it for one already running,
  and a browser stays signed in for 30 days, across restarts. `[web]
  trust_local = true` brings back the old behaviour.
- The sign-in form posts the token instead of putting it in the address, a
  session can be ended (Setup has Sign out and Sign out everywhere), the
  page's script runs under a per-response nonce instead of `'unsafe-inline'`,
  responses say `Referrer-Policy: no-referrer`, and a short `--token` is warned
  about.
- A new data folder is created 0700, and `furmi doctor` flags one that other
  users can read.
- Tasks no longer inherit furmi's own notification tokens, and `[runner]
  strip_env` names more variables to keep from them. A prompt that starts with
  `-` can't be read as an option by the agent's own parser, and `{prompt_file}`
  gives a launcher the prompt in a 0600 file instead of on its command line.
- `on_finish` refuses to commit a new file that looks like a secret (`.env`, a
  private key, ...) or is over 10 MB, and leaves the work where it is
  (`commit_guard = false` under `[defaults]` turns it off).
- `install.sh` checks the installer against the release's `sha256.sum` when
  it is listed there and takes `FURMI_VERSION`; CI lints it, and a CodeQL
  workflow analyzes the code and the workflows.

Agents and tasks

- Headless agents had no approval flags, so one that asks before editing
  finished its first task having changed nothing. `furmi init` now asks how far
  the default agent may go, `furmi agents allow <agent> <ask|edits|all>` and
  Setup change it, the answer lives under `[unattended]`, and `furmi doctor`
  warns while it is unanswered.
- New task fields, also options of `furmi add`: `base` (what a new branch
  starts from, fetched first), `setup` (a command run in the workspace before
  the first attempt), `after` (tasks that must finish as done first) and
  `priority`. `[defaults]` takes `base` and `setup`; the dashboard's form has
  them under More options.
- A task, schedule or template can carry a `title`: one short line (max 120
  chars) saying what it is doing, shown in a `TITLE` column in `furmi ls`, at
  the top of a dashboard row and as the heading of a task's own view. It is
  optional and falls back to the first line of the prompt, so existing files
  read as they did. `furmi add --title`, `furmi schedule add|edit --title`, the
  dashboard's New task and Edit forms write it, and the new `furmi title <id>
  "<text>"` changes it on any task, waiting or finished (an empty text takes it
  away). The `furmi-tasks` skill tells agents to always pass one.
- `[runner] daily_token_budget` holds the queue once today's finished tasks have
  used that many tokens; `furmi status` and Setup show it.
- A task opened from `on_finish: pr` records its pull request, which the task
  view links to.

Dashboard

- A task's view shows the end of what a failed run printed, what its branch
  changed, the pull request, and what it waits for.
- The log viewer follows colour codes, wraps or doesn't, copies and downloads,
  has an attempt picker and a Jump to the latest button.
- Times read as "3 min ago" or "in 2 h", with the exact time under them and on
  hover. A tab in the background shows a count in its title and icon, and Setup
  can turn on browser notifications for tasks that end while it is hidden.
- Questions are asked in the page rather than in the browser's own dialogs,
  default to the safe answer, and Dropping a waiting task can be undone.
  Dialogs take focus, keep Tab inside and hand focus back.
- Setup is wider and says things plainly: how far each agent may go,
  notifications (with a test message), a daily token budget, the other
  machines, and signing out. Repeating shows each schedule's last run and has a
  Run now button (also `furmi schedule run`). A link to a task opens the list
  that holds it; one to a missing task, or an address that isn't there, says so
  and leads back. The header keeps Setup on a phone.
- Saving another machine's task as a template works.
- Fixed: a prompt over 16 KB could not be queued from the form (the limit is
  now 1 MB and says so); `n` typed an `n` into the form it opened.
- The dashboard's Housekeeping banner now has buttons. A suggestion or a task
  that hit its requeue cap gets a **Retry** or **Delete branch** button, and
  **Fix all** in the banner runs every one. Branches go with `git branch -d`,
  so git still refuses an unmerged one, and only fixes the last pass offered
  are accepted.
- The dashboard's task form can set a task's own options, as `furmi add` can:
  **Mode** (`headless`/`interactive`), **Workspace** (its own worktree, or the
  project folder itself), **Branch**, **Retries**, **Wait between retries** and
  **Give up after**. They are there for a new task, an edit of a waiting one
  and a schedule alike, and a value that doesn't make sense is refused the way
  the flags are; leaving one empty follows `[defaults]`.
- Choosing **Any machine (least busy)** in the dashboard's task form suggests
  models again. The box used to be left out of it: no machine is picked until
  the task is queued, so there was no CLI to ask. Now every machine that could
  take the task is asked for what the agent it would run with can run, all at
  once (nothing waits on a slow machine, and a project narrows it to the
  machines that have a clone), and the box offers what they can between them. A
  machine whose default agent can't be handed a model takes the box away, so a
  pinned model can't land somewhere it can't be used.

Command line

- `furmi add "- a list"` and `--prompt "- ..."` are accepted; a lone mistyped
  flag is still an error.
- `furmi ls --json` and `furmi status --json`, `furmi completions <shell>`, a
  `--help` that groups the commands, and shorter project paths in `furmi ls`.
- `furmi models [--agent X] [--refresh]`: what each configured agent's CLI says
  it can run, from a terminal. It is the same list the dashboard's task form
  offers (one report, shared with `GET /api/models`), with a plain reason for
  the agents that have none: no `models_cmd` in `config.toml`, a command that
  failed, a name the config doesn't define. `--refresh` asks the CLIs again and
  rewrites `models-cache.json`; without it a cached answer from the last hour is
  reused.

- The dashboard's sidebar has two sections: **Queue** for the lists work moves
  through, and **Library** for the ones kept for later. Drafts and Templates
  moved down to Library, where nothing is running and nothing is waiting.
- Select mode works on **Repeating** and **Templates** too, not only task
  lists. Tick rows (or the header box for all of them) and the bar offers what
  they all allow: **Run now**, **Pause** or **Resume**, and **Delete** for
  repeating tasks; **Run now** and **Delete** for templates.
- **Run now** adds one task from a repeating task or a template at once, from
  its row or the bar. A repeating task keeps its own times, and one already
  queued or running is not doubled unless it allows overlap. A template needs a
  project to say where to run.

## 0.8.0

- A dashboard serving other machines no longer refuses a browser with one line
  of plain text. A request that wants a page (no session yet, an expired one,
  or a `Host` the dashboard doesn't answer to) gets a page that says what's
  wrong and how to get back in: a box to paste the token into, the `cat`
  command for the token file on the box, the address with `?token=` already in
  it, and, for a wrong name, the names it does answer to. The token never
  appears in it, a dead session cookie is cleared so the next reload starts
  clean, and `curl`, a proxy and `/api` callers keep the plain-text 401 and
  403. The page's ant is served without a token, so a refusal looks like the
  dashboard rather than a broken one.

## 0.7.0

- A worktree task that edits the base checkout instead of its own worktree no
  longer looks like a clean no-op. The runner saves the base checkout's dirty
  paths when an attempt starts and compares them when it ends; if the worktree
  stayed clean while the base checkout gained changes and the task has an
  `on_finish` step, its log names those paths and the dashboard's health list
  warns that the work may be in the shared checkout. The task still succeeds:
  the warning makes the escape visible, not fatal.

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

- Drafts: `furmi add --draft` (and **Save as draft** in the dashboard's New task
  form) saves a task without queueing it. The runner leaves it alone until you
  look it over, edit it if you like, and `furmi queue <id>` it (the dashboard's
  Queue button). Drafts have their own list in the dashboard. The `furmi-tasks`
  skill makes an agent ask whether to queue a task or leave a draft, unless you
  have already said. `furmi duplicate <id>` (Duplicate as draft in the task's
  detail view) copies any task into a new draft.

- Templates: tasks to start new ones from, in `templates/`. `furmi template
  ls|show|save|delete` manages them, `furmi add --template <id>` and the
  dashboard's Templates page start a task from one, and any task can be saved as
  one. The Templates page also edits a template and writes a new one from
  scratch. A few starter templates are created the first time furmi runs.

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
