# Rename plan: loompa-forge → Furmiculi

Proposed: project **Furmiculi**, binary **`furmi`**. Reasons for renaming: the
current name is a Willy Wonka pun that doesn't say what the tool does, and the
`lf` binary clashes with the well-known `lf` terminal file manager.

> **Applied on 2026-10-19** — code, tests, docs, the `furmi-tasks` skill
> (mindforge vault + pod symlinks), the forgepod mirror and the pod's own
> queue (moved to `~/.furmiculi`). Committed in this repo, the mindforge vault
> and forgepod. Everything behind an account — the GitHub repo rename, crate
> reservation, npm/PyPI and domains — is still open, as is cutting the first
> release under the new name.

*Furmiculi* is from Sicilian/Corsican *furmìcula* ("ant"), so "little ants":
a colony of agents working in parallel.

## Availability (checked 2026-10-19)

| | `furmiculi` | `furmi` |
|---|---|---|
| crates.io, npm, PyPI | free | free |
| RubyGems, Homebrew, Docker Hub, NuGet, Go | free | free |
| GitHub user/org name | free | **taken** (a private person) |
| Domains | `.com` `.dev` `.app` `.io` unregistered | `.dev` `.app` `.io` free; **`furmi.com` taken** (since 2008, parked) |
| Trademark | no hits (third-party US aggregator only) | same |

- No coding-agent, task-queue or dev-tool project uses either name.
- Near-neighbour: `furmint-*` crates (a game engine) on crates.io.
- Not checked: USPTO/EUIPO/UKIPO directly, `.sh`/`.co`/`.net`/`.org`
  domains, GitHub org names.
- Use the `furmiculi` GitHub org, not `furmi`.

Names ruled out (taken in the same space): nightshift, kiln, foreman,
conveyor. Other shortlisted: morrow (crate name taken), docket, smelt, grist
(not checked).

## Decisions (settled 2026-10-19)

- [x] Final project and binary names: `furmiculi` / `furmi`.
- [x] Home directory: `~/.furmiculi`.
- [x] No migration code and no compatibility shims: a **clean break**. The old
      `~/.loompa-forge` is not read, `LF_*` is not accepted, and no `lf` alias
      ships. Existing installs move their home by hand (in the pod: see the
      furmiculi block in forgepod's `entrypoint.sh` for the volume copy).

## Package and build

- [x] `Cargo.toml`: `name`, `repository`, `[[bin]] name = "lf"` → `furmi`;
      refresh `Cargo.lock`.
- [x] `dist-workspace.toml`: installer and updater names follow the package
      name (`furmiculi-installer.sh`, `furmiculi-update`).
- [x] `.github/workflows/ci.yml`, `release.yml`: any hard-coded binary or
      artifact names.
- [x] `e2e/package.json` (`loompa-forge-e2e`), `playwright.config.ts`,
      `e2e/tests/fixtures.ts` (`lf-e2e-` temp prefix).
- [ ] Reserve the crate: publish a placeholder to crates.io once the name is final.
      (Not done: needs registry credentials.)

## Code strings

- [x] `src/update_check.rs`: `APP_NAME`, the `loompa-forge-update` message,
      the releases URL.
- [x] `src/home.rs`: default `~/.loompa-forge`, doc comments, the
      `.loompa-forge-test` test.
- [x] `src/config.rs`: default tmux session `"loompa"`, config template
      header, assertion at ~line 576; `tests/runner.rs:917`
      (`losing_the_loompa_session_requeues_the_task`).
- [x] `src/doctor.rs`: systemd unit name and launchd label `dev.loompa-forge`
      (~lines 340–370).
- [x] `src/cmd.rs:144` banner and the user-facing `lf ...` hints; `lf`
      mentions in `src/skill.rs`, `src/run.rs`, `src/git.rs`, `src/web.rs`.
- [x] `src/main.rs`: clap `name`, `--home` help text.
- [x] `src/web_dashboard.html` / `src/web.rs`: `lf-token` meta tag and
      `__LF_TOKEN__` placeholder.

## Environment variables

- [x] `LF_HOME`, `LF_BIN`, `LF_TASK_ID`, `LF_NO_UPDATE_CHECK` → `FURMI_*`.
- [x] `LF_E2E_CONFIG` in `scripts/e2e-agents.sh`.

## Agent skill and docs

- [x] Rename the `lf-tasks` skill to `furmi-tasks`: `assets/SKILL.md`,
      `assets/AGENTS.md`, `src/skill.rs`, `docs/FORMAT.md`, and the symlinks in
      `~/.agents/skills` and `~/.claude/skills`. Old symlinks need cleaning up
      (in `init`, or at least a changelog note).
- [x] The `lf-tasks` skill in the mindforge vault, if copied from this project.
- [x] `README.md` (~69 `lf` mentions plus install URLs), `docs/FORMAT.md`
      (~43), `TODO.md`, the `AGENTS.md` template.
- [x] Tests: `tests/cli.rs` (~64 mentions) and `tests/runner.rs` (~43) call
      the binary by name.

## GitHub and distribution

- [ ] Rename the repository (`giannileggio/loompa-forge`). GitHub redirects the
      old URL; released binaries point at the old installer/updater path, so
      keep the redirect alive. (Remote now resolves to `furmiculi` and the local
      clone was moved to `~/Projects/furmiculi` on 2026-10-20; only the GitHub
      account side — settings, description, redirect check — is outstanding.)
- [ ] Create the `furmiculi` GitHub org if wanted.
- [ ] Last release under the old name: add a notice pointing to the new name.
- [ ] Cut a release under the new name; check `furmiculi-update` upgrades from
      the final `loompa-forge` release.
- [ ] Register domains (`furmiculi.dev` or `.com`) and reserve the npm/PyPI
      names if wanted.

## Pod config (forgepod)

- [x] Anything outside `~/Projects` that references `lf` (shell aliases,
      `dotfiles/`, mise config, agent skills in forgepod) — update in the
      forgepod repo.

## Verification

- [x] `grep -rIn 'loompa\|\blf\b\|LF_'` returns only intended leftovers
      (migration code, changelog).
- [x] `cargo test`, the e2e suite, and one manual `furmi add` → `furmi start`.
- [ ] Install from the new installer script on a clean machine or container.
      Blocked on the first release under the new name.
