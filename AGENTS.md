# AGENTS.md

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a PR: it is the full
pre-PR checklist. This file is only the part an agent needs before touching the
code, and it restates nothing.

## Before you call it done

Run the checklist from CONTRIBUTING.md, verbatim:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo deny check          # dependency policy (deny.toml), if you changed Cargo.toml
shellcheck --severity=error scripts/*.sh
shellcheck --severity=warning install.sh
```

The browser suite is separate, and needs a built binary first:

```sh
cargo build                 # browser tests for the dashboard, in e2e/
cd e2e && npm ci && npx playwright install chromium
npm test                    # FURMI_BIN=path/to/furmi to test another build
```

`scripts/e2e-local.sh` runs those stages in order. CONTRIBUTING.md has that, the
`rust-version` CI checks, and the release check.

## The dashboard rule

`src/web_dashboard.html` is a single file embedded in the binary. It has no build
step and no linter, so nothing catches a broken page except the browser tests in
`e2e/` — they are its only checks. If you edit it, run them; do not add a build
step, a bundler or a linter to it.
