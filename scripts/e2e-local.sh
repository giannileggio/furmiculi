#!/usr/bin/env bash
# The daily "does everything still work" run, on this machine.
#
#   scripts/e2e-local.sh [--agents pi,opencode] [--release] [--fresh] [--no-browser]
#
# Stages, in order, each reported separately and none aborting the rest:
#
#   build      cargo build (debug, or --release)
#   rust       cargo test --all: the CLI and runner tests in tests/, which drive
#              the built binary against real git and tmux
#   browser    the dashboard in Chromium through Playwright (e2e/)
#   agents     the real coding agents through scripts/e2e-agents.sh, with
#              --agents; they cost tokens, so this is opt-in
#
# Everything runs against throwaway homes, repos and tmux servers, so none of
# your tasks, worktrees or tmux windows are touched. The output is kept in
# ${XDG_CACHE_HOME:-~/.cache}/furmi-e2e/last.log, which is what a timer or a
# furmi schedule has to show when it fails.
#
# Exit status is non-zero if any stage failed. `--fresh` reinstalls the browser
# suite's npm and browser downloads; `--no-browser` skips that stage where there
# is no node.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}/furmi-e2e
LOG=$CACHE/last.log
AGENTS=""
FRESH=0
BROWSER=1
PROFILE=debug

while [ $# -gt 0 ]; do
  case $1 in
    --agents)
      AGENTS=${2:-}
      shift 2
      ;;
    --agents=*)
      AGENTS=${1#*=}
      shift
      ;;
    --release) PROFILE=release; shift ;;
    --fresh) FRESH=1; shift ;;
    --no-browser) BROWSER=0; shift ;;
    -h | --help)
      sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      exit 2
      ;;
  esac
done

mkdir -p "$CACHE"
# Keep every stage's output, not only the summary a timer would otherwise get.
exec > >(tee "$LOG") 2>&1

# A task running this from inside furmi has FURMI_TASK_ID set; the nested runs
# in the stages below must not think they are that task.
unset FURMI_TASK_ID

FURMI_BIN=$ROOT/target/$PROFILE/furmi

missing=
for tool in cargo git tmux; do
  command -v "$tool" >/dev/null || missing="$missing $tool"
done
if [ "$BROWSER" = 1 ]; then
  for tool in npm npx; do
    command -v "$tool" >/dev/null || missing="$missing $tool"
  done
fi
if [ -n "$missing" ]; then
  echo "missing:$missing" >&2
  exit 2
fi

echo "furmiculi e2e: $FURMI_BIN"
echo "log: $LOG"
date

PASS=0
FAIL=0
RESULTS=()

run_stage() {
  local name=$1
  local start=$SECONDS
  shift
  echo
  echo "=== $name"
  if "$@"; then
    PASS=$((PASS + 1))
    RESULTS+=("PASS  $name  ($((SECONDS - start))s)")
  else
    local status=$?
    FAIL=$((FAIL + 1))
    RESULTS+=("FAIL  $name  ($((SECONDS - start))s, exit $status)")
  fi
}

stage_build() {
  if [ "$PROFILE" = release ]; then
    cargo build --release
  else
    cargo build
  fi
}

stage_rust() {
  if [ "$PROFILE" = release ]; then
    cargo test --release --all
  else
    cargo test --all
  fi
}

stage_browser() {
  local e2e=$ROOT/e2e
  if [ "$FRESH" = 1 ] || [ ! -d "$e2e/node_modules" ]; then
    (cd "$e2e" && npm ci) || return 1
  fi
  # A no-op once this suite's Chromium is downloaded.
  (cd "$e2e" && npx playwright install chromium) || return 1

  # Chromium's headless shell is linked against system libraries a bare
  # container or runner lacks; e2e-deps.sh fetches them without root and does
  # nothing when they are already there.
  local libs=
  if [ "$(uname -s)" = Linux ]; then
    "$ROOT/scripts/e2e-deps.sh" || return 1
    libs=$("$ROOT/scripts/e2e-deps.sh" --print-only)
  fi
  local libpath=$libs
  if [ -n "${LD_LIBRARY_PATH:-}" ]; then
    libpath=${libpath:+$libpath:}$LD_LIBRARY_PATH
  fi

  # CI's settings on purpose: two workers and no retries, the way the push CI
  # runs it. On a busy machine Playwright's default worker count finds races
  # in the tests' own assumptions, and an unattended daily run should be a
  # verdict, not a lottery.
  (cd "$e2e" && CI=1 LD_LIBRARY_PATH="$libpath" FURMI_BIN="$FURMI_BIN" npm test)
}

stage_agents() {
  # Run from inside an agent session (as the daily furmi schedule does not,
  # but a person might), the agents under test must not inherit that session:
  # pi would resume or write the outer session's file.
  unset PI_SESSION_FILE PI_SESSION_ID PI_CODING_AGENT PI_REASONING_LEVEL \
    CLAUDECODE CLAUDE_CODE_ENTRYPOINT CLAUDE_CODE_SSE_PORT

  # The built-in agent presets, not whatever ~/.furmiculi/config.toml overrides:
  # this checks the flags the binary ships with.
  local config=${FURMI_E2E_CONFIG:-$ROOT/scripts/e2e-agents-config.toml}
  FURMI="$FURMI_BIN" FURMI_E2E_CONFIG="$config" \
    "$ROOT/scripts/e2e-agents.sh" --agents "$AGENTS"
}

run_stage build stage_build
run_stage rust-tests stage_rust
if [ "$BROWSER" = 1 ]; then
  run_stage browser-tests stage_browser
fi
if [ -n "$AGENTS" ]; then
  run_stage agent-tests stage_agents
fi

echo
printf '%s\n' "${RESULTS[@]}"
echo
echo "passed: $PASS  failed: $FAIL"
echo "log: $LOG"
[ "$FAIL" = 0 ]
