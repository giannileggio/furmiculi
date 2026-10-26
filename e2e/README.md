# Browser tests for the dashboard

Playwright drives the real dashboard — `furmi web` / `furmi start`, the HTML it
serves, the actions it posts — the way a person would: clicking rows, filling
the form, pressing keys. The Rust tests cover the routes, the token and host
rules and the files behind them; none of them render a page or click a button,
so this is where the dashboard's own behaviour is tested.

## Running them

```sh
cargo build                              # the binary under test
cd e2e
npm ci
npx playwright install chromium          # the browser (add --with-deps with root, see below)
npm test                                 # FURMI_BIN=path/to/furmi for another build
```

Each test gets its own `furmi` process, home, git repo, tmux server and port
(see `tests/fixtures.ts`), so they run in parallel and touch nothing of yours:
no `~/.furmiculi`, no real tmux session, no network. The port is one `furmi`
asks the OS for (`--port 0`) and reports back, rather than one the test guessed
and hoped stayed free until `furmi` bound it. The agent is a stub that runs
the task's prompt as a shell command, so no coding agent is installed or
needed — but `tmux` and `git` are. Tests that need the queue running opt in with
`test.use({ runner: true })`.

## In a container, or anywhere there is no root

`npx playwright install --with-deps chromium` is the supported way, and it needs
root for the `--with-deps` half. Chromium's headless shell is dynamically
linked against the system's X11/AT-SPI/Gbm libraries, though, and a bare image
often has none of them — the browser then dies with `libnspr4.so: cannot open
shared object file` before a single test starts.

For that case, `scripts/e2e-deps.sh` fetches those libraries as .deb packages
from the distribution's archive, unpacks them into a directory of their own, and
prints the path:

```sh
scripts/e2e-deps.sh            # into ${XDG_CACHE_HOME:-~/.cache}/furmi-e2e-libs
LD_LIBRARY_PATH=$(scripts/e2e-deps.sh --print-only) FURMI_BIN=../target/debug/furmi npm test
```

Nothing is installed and nothing outside that directory is touched: no root, no
apt lock, no packages added or upgraded. It checks the browser it finds and
does nothing when nothing is missing; `--refresh` fetches again after a browser
update (a new Chromium can want more libraries, and it says which are still
missing rather than leaving you with a crash).

## Adding a test

`tests/fixtures.ts` has the pieces: `forge.add(id, prompt, ...furmi add flags)`
queues a task, `forge.furmi(...)` runs any command, `forge.taskFile(id)` finds a
task's file (so a test can rewrite it to be running, failed or finished),
`openDashboard(page, forge)` waits for the first state load, and `row`/`tab`/
`createdId` find what you would click. `test.use({ config: "..." })` appends to
the generated `config.toml`.

Two habits worth keeping: assert on what a person would see (text, badges,
`aria-sort`, the log body) rather than on markup, and give things a name the
test can find — a new button or column needs a label anyway.

## When one fails

`retries` is deliberately 0: a flake is a bug in the dashboard or in the test,
and hiding it makes the suite worth less. To tell a flake from a real failure,
run the suspect test on its own, then repeatedly:

```sh
npx playwright test tests/queue.spec.ts:6
npx playwright test tests/queue.spec.ts:6 --repeat-each=10
npx playwright test tests/queue.spec.ts:6 --repeat-each=10 --workers=1
```

A failure only under parallel load is usually a race in the test's assumptions
(the dashboard polls every 2s) rather than in what it asserts. CI keeps the HTML
report and traces of failures as artifacts.
