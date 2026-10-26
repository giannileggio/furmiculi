# Contributing

Issues and pull requests are welcome. For anything bigger than a small fix,
open an issue first so we can agree on the shape.

Security problems are not issues: see [SECURITY.md](SECURITY.md).

## Before you open a PR

CI runs these, and so should you:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo deny check          # dependency policy (deny.toml), if you changed Cargo.toml
shellcheck --severity=error scripts/*.sh
```

The code supports the Rust version in `rust-version` in `Cargo.toml` (CI
checks it).

Use a short, descriptive PR title: it becomes the squash-commit subject and the
release notes. Put the detail in the description.

The dashboard is one HTML file, `src/web_dashboard.html`, embedded in the
binary. It has no build step or linter; the browser tests below are its checks.

## Testing

```sh
cargo test                  # unit tests, plus the CLI and runner tests in tests/
                            # (they run the built `furmi` against real git and tmux)

cargo build                 # browser tests for the dashboard, in e2e/
cd e2e && npm ci && npx playwright install chromium
npm test                    # FURMI_BIN=path/to/furmi to test another build
```

See [e2e/README.md](e2e/README.md) for what those cover, how to add one, and
what to do where there is no root to install the browser's system libraries
(`scripts/e2e-deps.sh`).

The browser tests start `furmi start`/`furmi web` for each test against a throwaway
home, git repo and tmux server, with a stub agent that runs the prompt as a
shell command, so they need `tmux` and `git` but no real coding agent and touch
none of your files. CI runs both on every push and pull request.
