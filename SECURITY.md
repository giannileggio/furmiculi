# Security

furmi runs coding agents, unattended, with whatever credentials those agents
already have. It is a tool for your own machine, not a sandbox, and it assumes
`~/.furmiculi` (or `$FURMI_HOME`) is yours.

## Reporting a problem

Open a [private security advisory](https://github.com/giannileggio/furmiculi/security/advisories/new)
on GitHub. If you can't, mail giannileggio@gmail.com. Please don't open a
public issue first.

This is a spare-time project: expect an answer within a week or so, and a fix
in the next release rather than a same-day patch.

## What furmi holds

Nothing of its own. It has no account, no server, and no telemetry — the only
network call is the once-a-day release check, which you can turn off with
`FURMI_NO_UPDATE_CHECK=1`.

What it *uses*:

- **Your agents' credentials.** `claude`, `codex`, `pi` and the rest run as
  you, with the logins they already have. furmi doesn't read or store them.
- **A GitHub token, sometimes.** `furmi update` uses `GITHUB_TOKEN`, `GH_TOKEN`
  or whatever `gh auth token` returns, when one is there, only to talk to the
  GitHub API. `on_finish: push` / `pr` shell out to `git` and `gh`, which use
  their own credentials.
- **`web-token`**, a 128-bit secret it generates for the dashboard, in
  `~/.furmiculi/web-token` (0600 — a group- or world-readable one is refused).

## What counts as trusted

- **Task and schedule files are code.** A task's body is handed to an agent
  CLI, and `on_finish` runs `git` and `gh`; a schedule's `cron` decides when.
  Anyone who can write those files can already run commands as you, so don't
  accept task files from people you wouldn't give a shell.
- **The dashboard is local by default.** `furmi web` / `furmi start` bind
  127.0.0.1 and additionally require a loopback `Host` (against DNS
  rebinding), a per-session token on actions (against cross-site forgery) and
  a sign-in: a secret only the data folder's owner can read
  (`.local-token`, 0600), which `furmi open` gives to a browser once. Without
  it any program on the machine, of any user, could fetch the page and queue a
  task that runs as you; `[web] trust_local = true` accepts that.
- **Serving other machines is opt-in**, and then the bearer token is the gate:
  a `--allowed-host '*'` entry means any `Host` is accepted and the token is
  the only thing left. It speaks plain HTTP — put `tailscale serve` or an SSH
  tunnel in front of it, not the open internet.
- **What a task inherits and commits is limited.** Tasks don't get furmi's own
  notification tokens (`[runner] strip_env` names more), an agent's prompt
  can't be read as an option, and `on_finish` refuses a new file that looks like
  a secret or is over 10 MB (`commit_guard`). These are guard rails against
  accidents, not a sandbox: an agent can still read anything you can.
- **Running an agent is running an agent.** A prompt-injected task can do
  whatever that agent can. Keep unattended agents on the shortest leash their
  own flags allow (`--permission-mode`, `--full-auto`, …), and see the
  `max_parallel` and `timeout` limits, and `[unattended]`, which says how far
  each agent may go with nobody to ask (`edits` before `all`) (see `furmi init`'s config.toml and
  [docs/FORMAT.md](docs/FORMAT.md)).

## In scope

- Getting past the dashboard's loopback, `Host`, `Origin` or token checks, or
  reading/writing files outside `$FURMI_HOME` through it.
- Leaking `web-token`, an agent's credentials, or a task's output to somewhere
  it shouldn't go (logs, argv, `ps`, a unit's journal).
- `furmi update` installing something other than the release it verified.
- Privilege escalation from a task file: writing to `$FURMI_HOME` or a repo
  should not be able to run as anyone but you.

## Out of scope

- What a coding agent does when a task asks it to do something — that's the
  agent's own security model, and furmi's job is only to launch it.
- Anything that needs write access to `$FURMI_HOME`, to a task file, or to your
  shell already.
- Missing hardening in an agent preset you can change in `config.toml` (those
  are documented defaults, not promises).
