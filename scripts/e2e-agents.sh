#!/usr/bin/env bash
# End-to-end check of furmi against real coding agents.
#
# Builds a throwaway git repo and a throwaway furmi home (agent definitions are
# copied from your real config.toml), queues a few tiny tasks with
# deterministic outcomes, runs the queue and checks every result.
#
#   scripts/e2e-agents.sh [--agents claude,pi,opencode|none] [--keep]
#
# Env:
#   FURMI              furmi binary to test            (default: furmi on $PATH)
#   FURMI_E2E_CONFIG   config.toml to copy agents from (default: ~/.furmiculi/config.toml)
#   FURMI_E2E_TIMEOUT  overall limit in seconds        (default: 1200)
#   PI_MODEL, OPENCODE_MODEL, CLAUDE_MODEL   models to use (empty = the agent's default)
#
# What runs where:
#   fake (always)      a stand-in agent: ok, fail, and a schedule firing exactly once
#   pi, opencode       write-file, fix-test, blocked, bad-model, timeout
#   claude             blocked, bad-model only: the claude preset has no permission
#                      flags, so a headless run can't edit files or run commands
#
# Tasks that must succeed get 2 retries (free tiers rate-limit); bad-model and
# timeout get none, so "not retried" means something.
#
# Exit status is non-zero if any check fails. Real agents cost tokens and time.
set -u

FURMI=${FURMI:-furmi}
AGENTS=claude,pi,opencode
KEEP=0
while [ $# -gt 0 ]; do
  case $1 in
    --agents) AGENTS=$2; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
[ "$AGENTS" = none ] && AGENTS=
PI_MODEL=${PI_MODEL-opencode-go/deepseek-v4.1-flash}
OPENCODE_MODEL=${OPENCODE_MODEL-opencode-go/deepseek-v4.1-flash}
CLAUDE_MODEL=${CLAUDE_MODEL-}
LIMIT=${FURMI_E2E_TIMEOUT:-1200}
SRC_CONFIG=${FURMI_E2E_CONFIG:-$HOME/.furmiculi/config.toml}

for tool in git tmux "$FURMI"; do
  command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 2; }
done

TMP=$(mktemp -d)
HOME_DIR=$TMP/home
REPO=$TMP/repo
SESSION=furmie2e-$$
export FURMI_HOME=$HOME_DIR

cleanup() {
  tmux kill-session -t "=$SESSION" 2>/dev/null
  if [ "$KEEP" = 1 ]; then echo "kept: $TMP"; else rm -rf "$TMP"; fi
}
trap cleanup EXIT

furmi() { "$FURMI" --home "$HOME_DIR" "$@"; }

# --- fixture ------------------------------------------------------------------

mkdir -p "$REPO"
git -C "$REPO" init -q -b main
git -C "$REPO" config user.email e2e@example.com
git -C "$REPO" config user.name "furmi e2e"
echo 41 > "$REPO/answer.txt"
cat > "$REPO/check.sh" <<'EOF'
#!/bin/sh
[ "$(cat answer.txt)" = 42 ]
EOF
echo "fixture" > "$REPO/README.md"
git -C "$REPO" add -A
git -C "$REPO" commit -q -m fixture

furmi init >/dev/null 2>&1
if [ -f "$SRC_CONFIG" ]; then
  cp "$SRC_CONFIG" "$HOME_DIR/config.toml"
fi
sed -i \
  -e "s/^tmux_session = .*/tmux_session = \"$SESSION\"/" \
  -e 's/^poll_interval = .*/poll_interval = "2s"/' \
  -e 's/^max_parallel = .*/max_parallel = 4/' \
  -e 's/^max_parallel_per_repo = .*/max_parallel_per_repo = 4/' \
  "$HOME_DIR/config.toml"
cat >> "$HOME_DIR/config.toml" <<'EOF'

[agents.fake]
headless = ["sh", "-c", "{prompt}"]
interactive = ["sh", "-c", "{prompt}"]
EOF
# Pi isn't a built-in preset; add it if the copied config lacks it.
grep -q '^\[agents\.pi\]' "$HOME_DIR/config.toml" || cat >> "$HOME_DIR/config.toml" <<'EOF'

[agents.pi]
headless = ["pi", "--print", "{prompt}"]
interactive = ["pi", "{prompt}"]
model_args = ["--model", "{model}"]
EOF

# --- helpers ------------------------------------------------------------------

PASS=0
FAIL=0
RESULTS=()
record() { # name ok(0/1) detail
  if [ "$2" = 0 ]; then PASS=$((PASS + 1)); RESULTS+=("PASS  $1"); else
    FAIL=$((FAIL + 1)); RESULTS+=("FAIL  $1: $3"); fi
}

field() { sed -n "s/^$2: *//p" "$HOME_DIR/archive/$1.md" 2>/dev/null | head -1; }
branch_file() { git -C "$REPO" show "furmi/$1:$2" 2>/dev/null; }
commits_ahead() { git -C "$REPO" rev-list --count "main..furmi/$1" 2>/dev/null || echo 0; }

# add <id> <agent> <model> <extra furmi-add args...>   (prompt on stdin)
# RETRIES (default 0) sets the attempts after a failure, with a short delay.
add() {
  local id=$1 agent=$2 model=$3
  shift 3
  local model_args=()
  [ -n "$model" ] && model_args=(--model "$model")
  furmi add --repo "$REPO" --id "$id" --agent "$agent" "${model_args[@]}" \
    --retries "${RETRIES:-0}" --retry-delay 10s "$@" >/dev/null || echo "could not queue $id" >&2
}

model_of() { case $1 in pi) echo "$PI_MODEL" ;; opencode) echo "$OPENCODE_MODEL" ;; claude) echo "$CLAUDE_MODEL" ;; esac; }
bad_model_of() { case $1 in pi) echo "opencode-go/furmi-e2e-no-such-model" ;; opencode) echo "opencode/furmi-e2e-no-such-model" ;; *) echo "furmi-e2e-no-such-model" ;; esac; }

# --- queue --------------------------------------------------------------------

add fake-ok fake "" --on-finish none <<'EOF'
exit 0
EOF
add fake-fail fake "" --on-finish none <<'EOF'
exit 3
EOF

cat > "$HOME_DIR/schedules/e2e-sched.md" <<EOF
---
id: e2e-sched
cron: "* * * * *"
repo: $REPO
agent: fake
retries: 0
on_finish: none
---

exit 0
EOF
furmi validate "$HOME_DIR/schedules/e2e-sched.md" >/dev/null || echo "schedule invalid" >&2

for agent in ${AGENTS//,/ }; do
  model=$(model_of "$agent")
  if [ "$agent" != claude ]; then
    RETRIES=2 add "$agent-write-file" "$agent" "$model" --on-finish commit <<'EOF'
Create a file named HELLO.txt at the repository root containing exactly one line: furmi-e2e-ok
Do nothing else.
EOF
    RETRIES=2 add "$agent-fix-test" "$agent" "$model" --on-finish commit <<'EOF'
`sh check.sh` currently fails. Make it pass by editing answer.txt. Do not edit check.sh.
Do nothing else.
EOF
    add "$agent-timeout" "$agent" "$model" --on-finish none --timeout 30s <<'EOF'
Run the shell command `sleep 300` and wait for it to finish. Do nothing else.
EOF
  fi
  RETRIES=2 add "$agent-blocked" "$agent" "$model" --on-finish none <<'EOF'
Read the file MISSING.md in the repository root and summarize it.
It does not exist: do not create it. Just say that it doesn't exist.
EOF
  add "$agent-bad-model" "$agent" "$(bad_model_of "$agent")" --on-finish none <<'EOF'
Print the word hello.
EOF
done

# --- run ----------------------------------------------------------------------

echo "running $(ls "$HOME_DIR/tasks" | wc -l) tasks (limit ${LIMIT}s) in $TMP"
start=$SECONDS
sched_removed=0
while :; do
  furmi run --once >/dev/null 2>&1
  if [ $sched_removed = 0 ] && grep -ql '^created_by: schedule:e2e-sched' "$HOME_DIR"/tasks/*.md "$HOME_DIR"/archive/*.md 2>/dev/null; then
    rm -f "$HOME_DIR/schedules/e2e-sched.md" # fired once; stop it firing again
    sched_removed=1
  fi
  left=$(ls "$HOME_DIR/tasks" 2>/dev/null | wc -l)
  if [ "$left" = 0 ] && [ $sched_removed = 1 ]; then break; fi
  if [ $((SECONDS - start)) -ge "$LIMIT" ]; then echo "time limit reached, $left task(s) still queued"; break; fi
  sleep 3
done
furmi run --once >/dev/null 2>&1

# --- checks -------------------------------------------------------------------

check_status() { # id expected
  local got
  got=$(field "$1" status)
  [ "$got" = "$2" ]; record "$1 is $2" $? "status was '${got:-<not archived>}': $(field "$1" error | head -c 200)"
}

check_status fake-ok done
check_status fake-fail failed
[ "$(field fake-fail exit_code)" = 3 ]; record "fake-fail keeps exit code 3" $? "exit_code was '$(field fake-fail exit_code)'"

n=$(grep -l '^created_by: schedule:e2e-sched' "$HOME_DIR"/archive/*.md 2>/dev/null | wc -l)
[ "$n" = 1 ]; record "schedule fired exactly once" $? "fired $n times"
sched_id=$(basename "$(grep -l '^created_by: schedule:e2e-sched' "$HOME_DIR"/archive/*.md 2>/dev/null | head -1)" .md)
[ -n "$sched_id" ] && [ "$(field "$sched_id" status)" = done ]; record "scheduled task ran to done" $? "status was '$(field "$sched_id" status)'"

for agent in ${AGENTS//,/ }; do
  if [ "$agent" != claude ]; then
    id=$agent-write-file
    check_status "$id" done
    [ "$(branch_file "$id" HELLO.txt | tr -d '[:space:]')" = furmi-e2e-ok ]; record "$id wrote HELLO.txt" $? "content was '$(branch_file "$id" HELLO.txt | head -c 80)'"
    [ "$(commits_ahead "$id")" -ge 1 ]; record "$id committed on its branch" $? "no commit on furmi/$id"

    id=$agent-fix-test
    check_status "$id" done
    [ "$(branch_file "$id" answer.txt | tr -d '[:space:]')" = 42 ]; record "$id fixed answer.txt" $? "answer.txt was '$(branch_file "$id" answer.txt | head -c 40)'"
    [ "$(branch_file "$id" check.sh)" = "$(git -C "$REPO" show main:check.sh)" ]; record "$id left check.sh alone" $? "check.sh changed"

    id=$agent-timeout
    check_status "$id" failed
    field "$id" error | grep -qi 'time'; record "$id error mentions the timeout" $? "error was '$(field "$id" error | head -c 200)'"
    tmux list-windows -t "=$SESSION" -F '#{window_name}' 2>/dev/null | grep -qx "$id"
    [ $? -ne 0 ]; record "$id window was cleaned up" $? "window still exists"
  fi

  id=$agent-blocked
  check_status "$id" done
  [ "$(commits_ahead "$id")" = 0 ]; record "$id changed nothing" $? "$(commits_ahead "$id") commit(s) on furmi/$id"

  id=$agent-bad-model
  check_status "$id" failed
  [ "$(field "$id" attempts)" = 1 ]; record "$id was not retried" $? "attempts was '$(field "$id" attempts)'"
  # Informational: furmi recognises a provider rejection by its wording.
  if field "$id" error | grep -qi 'provider'; then
    RESULTS+=("info  $id: failure classified as a provider error")
  else
    RESULTS+=("info  $id: not recognised as a provider error ($(field "$id" error | head -c 120))")
  fi
done

# --- report -------------------------------------------------------------------

echo
printf '%s\n' "${RESULTS[@]}"
echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" = 0 ]
