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
  rebinding) and a per-run token on actions (against cross-site forgery).
- **Serving other machines is opt-in**, and then the bearer token is the gate:
  a `--allowed-host '*'` entry means any `Host` is accepted and the token is
  the only thing left. It speaks plain HTTP — put `tailscale serve` or an SSH
  tunnel in front of it, not the open internet.
- **Running an agent is running an agent.** A prompt-injected task can do
  whatever that agent can. Keep unattended agents on the shortest leash their
  own flags allow (`--permission-mode`, `--full-auto`, …), and see the
  `[runner]` limits in [docs/FORMAT.md](docs/FORMAT.md) (`daily_budget_usd`,
  `max_parallel`, `timeout`).

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
