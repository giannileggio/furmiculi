#!/usr/bin/env bash
# End-to-end check of a released furmi: what `curl ... install.sh | sh` gives you.
#
# Installs through install.sh into a throwaway $HOME, runs a task through the
# installed binary with a stub agent (so no coding agent and none of your tokens
# are used), checks `furmi doctor`, then moves to the previous release and back
# with `furmi update`. That covers the installer, the archive contents, the
# install receipt and the update path: none of which `cargo test` can see.
#
#   scripts/e2e-release.sh [--keep] [--version V] [--previous V]
#
# Env:
#   FURMI_E2E_TIMEOUT        seconds to wait for the task        (default 300)
#   FURMICULI_INSTALLER_URL  install this installer instead of the latest release
#   GITHUB_TOKEN / GH_TOKEN  lifts GitHub's anonymous API rate limit
#
# Needs curl, python3, git, tmux and the network. Nothing outside a temp folder
# is touched: no ~/.furmiculi, no real tmux server, no install receipt of yours.
# Exit status is non-zero if any check fails.
set -uo pipefail

REPO_SLUG=giannileggio/furmiculi
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TIMEOUT=${FURMI_E2E_TIMEOUT:-300}

KEEP=0
LATEST=""
PREVIOUS=""
while [ $# -gt 0 ]; do
  case $1 in
    --keep) KEEP=1; shift ;;
    --version) LATEST=$2; shift 2 ;;
    --previous) PREVIOUS=$2; shift 2 ;;
    -h | --help)
      sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      exit 2
      ;;
  esac
done

for tool in curl python3 git tmux; do
  command -v "$tool" >/dev/null || {
    echo "missing: $tool" >&2
    exit 2
  }
done

TMP=$(mktemp -d)
export HOME=$TMP/home
mkdir -p "$HOME" "$TMP/tmux"
unset CARGO_HOME TMUX FURMI_HOME
# Not the user's tmux server, and no update notice mixed into what we parse.
export TMUX_TMPDIR=$TMP/tmux
export FURMI_NO_UPDATE_CHECK=1
SESSION=furmie2e-release-$$
REPO=$TMP/repo
BIN=

cleanup() {
  tmux kill-session -t "=$SESSION" 2>/dev/null
  if [ "$KEEP" = 1 ]; then
    echo "kept: $TMP"
  else
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

# --- helpers ------------------------------------------------------------------

PASS=0
FAIL=0
RESULTS=()
record() { # name ok(0/1) detail
  if [ "$2" = 0 ]; then
    PASS=$((PASS + 1))
    RESULTS+=("PASS  $1")
  else
    FAIL=$((FAIL + 1))
    RESULTS+=("FAIL  $1${3:+: $3}")
  fi
}

note() { RESULTS+=("info  $1"); }

step() { echo "--- $1"; }

github_get() {
  local url=$1 token=${GITHUB_TOKEN:-${GH_TOKEN:-}}
  if [ -n "$token" ]; then
    curl -fsSL -H "Authorization: Bearer $token" -H "Accept: application/vnd.github+json" "$url"
  else
    curl -fsSL -H "Accept: application/vnd.github+json" "$url"
  fi
}

# Release tags that carry the installer, newest first. A tag without it is one
# the update path can't use (the pre-rename releases), so it is not "previous".
tags_with_installer() {
  python3 -c '
import json, sys
for release in json.load(sys.stdin):
    if release["draft"] or release["prerelease"]:
        continue
    assets = {a["name"] for a in release.get("assets", [])}
    if "furmiculi-installer.sh" in assets:
        print(release["tag_name"])
'
}

field() { sed -n "s/^$2: *//p" "$HOME/.furmiculi/archive/$1.md" 2>/dev/null | head -1; }
version_of() { "$BIN" --version 2>/dev/null | awk '{print $NF}'; }
no_v() { echo "${1#v}"; }

# A task's branch, as `git show` reads it, e.g. `branch_file smoke hello.txt`.
branch_file() { git -C "$REPO" show "furmi/$1:$2" 2>/dev/null; }

# --- which releases -----------------------------------------------------------

step "reading releases from GitHub"
if [ -z "$LATEST" ]; then
  if ! json=$(github_get "https://api.github.com/repos/$REPO_SLUG/releases/latest"); then
    echo "could not read $REPO_SLUG's latest release (network, or the API rate limit)" >&2
    exit 2
  fi
  LATEST=$(python3 -c 'import json, sys; print(json.load(sys.stdin)["tag_name"])' <<<"$json")
  if [ -z "$LATEST" ]; then
    echo "no latest release for $REPO_SLUG" >&2
    exit 2
  fi
fi

if [ -z "$PREVIOUS" ]; then
  if json=$(github_get "https://api.github.com/repos/$REPO_SLUG/releases?per_page=30"); then
    PREVIOUS=$(tags_with_installer <<<"$json" | grep -vx "$LATEST" | head -1)
  fi
fi

echo "latest: $LATEST   previous: ${PREVIOUS:-<none>}"

# --- install ------------------------------------------------------------------

step "install.sh $LATEST into $HOME"
if out=$(FURMI_NO_SKILL=1 sh "$ROOT/install.sh" 2>&1); then
  record "install.sh succeeded" 0
else
  record "install.sh succeeded" 1 "$(tail -3 <<<"$out" | tr '\n' ' ')"
fi

BIN=$(find "$HOME" -maxdepth 5 -name furmi -type f -perm -0100 2>/dev/null | head -1)
if [ -z "$BIN" ]; then
  echo "install.sh left no furmi binary under $HOME" >&2
  printf '%s\n' "${RESULTS[@]}"
  exit 1
fi
echo "installed: $BIN"

got=$(version_of)
[ "$got" = "$(no_v "$LATEST")" ]
record "installed binary is $LATEST" $? "reported '$got'"

service_out=$("$BIN" service systemd 2>&1)
grep -q 'ExecStart=.*furmi' <<<"$service_out"
record "the generated service unit runs this binary" $? "unit had no ExecStart for furmi"

"$BIN" init --agent claude --no-global-skill >/dev/null 2>&1
record "furmi init on a fresh home" $? "init failed"
cat >>"$HOME/.furmiculi/config.toml" <<'EOF'

[agents.stub]
headless = ["sh", "-c", "{prompt}"]
interactive = ["sh", "-c", "{prompt}"]
EOF
"$BIN" agents use stub >/dev/null 2>&1
record "furmi agents use stub" $? "could not pick the stub agent"
# -i.bak is the one form both GNU and BSD sed accept, then drop the copy.
sed -i.bak \
  -e "s/^tmux_session = .*/tmux_session = \"$SESSION\"/" \
  -e 's/^poll_interval = .*/poll_interval = "1s"/' \
  "$HOME/.furmiculi/config.toml"
rm -f "$HOME/.furmiculi/config.toml.bak"

# --- run a task ---------------------------------------------------------------

step "queue and run one task"
mkdir -p "$REPO"
git -C "$REPO" init -q -b main
git -C "$REPO" config user.email e2e@example.com
git -C "$REPO" config user.name "furmi e2e"
echo fixture >"$REPO/README.md"
git -C "$REPO" add -A
git -C "$REPO" commit -q -m fixture

"$BIN" add --repo "$REPO" --id smoke --agent stub --on-finish commit \
  --retries 0 --retry-delay 1s --timeout 2m \
  "echo furmi-e2e-ok > hello.txt" >/dev/null 2>&1
record "furmi add queued the task" $?

start=$SECONDS
while [ ! -f "$HOME/.furmiculi/archive/smoke.md" ]; do
  "$BIN" run --once >/dev/null 2>&1
  [ $((SECONDS - start)) -ge "$TIMEOUT" ] && break
  sleep 2
done
"$BIN" run --once >/dev/null 2>&1

[ -f "$HOME/.furmiculi/archive/smoke.md" ]
record "the task finished and was archived" $? "still in $HOME/.furmiculi/tasks after ${TIMEOUT}s"

got=$(field smoke status)
[ "$got" = "done" ]
record "the task archived as done" $? "status was '${got:-<not archived>}': $(field smoke error | head -c 200)"

[ "$(branch_file smoke hello.txt | tr -d '[:space:]')" = furmi-e2e-ok ]
record "the task's commit is on furmi/smoke" $? "hello.txt was '$(branch_file smoke hello.txt | head -c 80)'"

"$BIN" doctor >/dev/null 2>&1
record "furmi doctor passes on the installed binary" $? "doctor reported a problem"

# --- update there and back ----------------------------------------------------

if [ -z "$PREVIOUS" ]; then
  note "no previous release with an installer: the update path is not checked"
else
  step "furmi update --to $PREVIOUS"
  if out=$("$BIN" update --to "$(no_v "$PREVIOUS")" 2>&1); then
    got=$(version_of)
    [ "$got" = "$(no_v "$PREVIOUS")" ]
    record "downgrade to $PREVIOUS" $? "reported '$got': $(tail -3 <<<"$out" | tr '\n' ' ')"
  else
    record "downgrade to $PREVIOUS" 1 "$(tail -3 <<<"$out" | tr '\n' ' ')"
  fi

  step "furmi update (back to latest)"
  if out=$("$BIN" update 2>&1); then
    got=$(version_of)
    [ "$got" = "$(no_v "$LATEST")" ]
    record "update back to $LATEST" $? "reported '$got': $(tail -3 <<<"$out" | tr '\n' ' ')"
  else
    record "update back to $LATEST" 1 "$(tail -3 <<<"$out" | tr '\n' ' ')"
  fi

  "$BIN" --version >/dev/null 2>&1
  record "the updated binary runs" $? "furmi --version failed after the update"
fi

# --- report -------------------------------------------------------------------

echo
printf '%s\n' "${RESULTS[@]}"
echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" = 0 ]
