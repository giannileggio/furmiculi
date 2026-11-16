#!/bin/sh
# Installs furmi with the release installer, then offers to make the
# furmi-tasks skill available in every project, so agents can queue tasks
# from any repo:
#
#   curl --proto '=https' --tlsv1.2 -LsSf \
#     https://raw.githubusercontent.com/giannileggio/furmiculi/main/install.sh | sh
#
# Set FURMI_NO_SKILL=1 to skip the question (scripted installs),
# FURMI_VERSION=0.8.0 to install that release instead of the latest, or
# FURMICULI_INSTALLER_URL to run another installer altogether.
#
# The installer checks the archive it downloads against the checksum it
# carries. The installer itself is checked against the release's sha256.sum
# when that file lists it; releases up to 0.8.0 list only the archives, so
# for those nothing is checked here. For more than a checksum,
# `gh attestation verify <file> --repo giannileggio/furmiculi` proves a
# downloaded archive was built by this repository's release workflow.
set -eu

RELEASES="https://github.com/giannileggio/furmiculi/releases"
if [ -n "${FURMI_VERSION:-}" ]; then
    BASE="$RELEASES/download/v${FURMI_VERSION#v}"
else
    BASE="$RELEASES/latest/download"
fi
INSTALLER="${FURMICULI_INSTALLER_URL:-$BASE/furmiculi-installer.sh}"

# Downloaded first, so a failed download fails the install instead of quietly
# running an empty script.
installer="$(mktemp "${TMPDIR:-/tmp}/furmi-install.XXXXXX")"
trap 'rm -f "$installer"' EXIT
curl --proto '=https' --tlsv1.2 -LsSf -o "$installer" "$INSTALLER" || {
    echo "furmi: could not download the installer from $INSTALLER" >&2
    exit 1
}
# sha256 of a file, with whichever tool this machine has.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# Only for the release's own installer: another URL has no sums to go with it.
if [ -z "${FURMICULI_INSTALLER_URL:-}" ]; then
    sums="$(curl --proto '=https' --tlsv1.2 -LsSf "$BASE/sha256.sum" 2>/dev/null || :)"
    want="$(printf '%s\n' "$sums" | awk '$2 == "furmiculi-installer.sh" || $2 == "*furmiculi-installer.sh" { print $1; exit }')"
    have="$(sha256_of "$installer")"
    if [ -n "$want" ] && [ -n "$have" ]; then
        if [ "$want" != "$have" ]; then
            echo "furmi: the installer doesn't match its release's sha256.sum; not running it" >&2
            echo "  expected $want" >&2
            echo "  got      $have" >&2
            exit 1
        fi
    fi
fi

# From /dev/null: stdin is this script, and the installer must not read it.
sh "$installer" "$@" < /dev/null

# The installer puts `furmi` on PATH for the next shell, not this one.
find_furmi() {
    if command -v furmi >/dev/null 2>&1; then
        command -v furmi
    elif [ -n "${CARGO_HOME:-}" ] && [ -x "$CARGO_HOME/bin/furmi" ]; then
        echo "$CARGO_HOME/bin/furmi"
    elif [ -n "${HOME:-}" ] && [ -x "$HOME/.cargo/bin/furmi" ]; then
        echo "$HOME/.cargo/bin/furmi"
    fi
}

# Asks on the terminal: stdin is this script, so reading it would eat the
# rest of the script.
ask() {
    printf '\n%s' "$1" 2>/dev/null > /dev/tty || return 1
    _answer=
    IFS= read -r _answer 2>/dev/null < /dev/tty || :
    case "$_answer" in
        y | Y | yes | YES | Yes) return 0 ;;
    esac
    return 1
}

# No terminal (CI, cron): say what to run instead of asking.
hint="To let agents queue tasks from any repo: furmi init --global-skill"

if [ "${FURMI_NO_SKILL:-0}" = "1" ]; then
    exit 0
fi

furmi_bin="$(find_furmi || :)"

if [ -z "$furmi_bin" ] || ! (: > /dev/tty) 2>/dev/null; then
    echo "$hint"
elif ask "Add the furmi-tasks skill everywhere, so agents can queue tasks from any repo? [y/N] "; then
    "$furmi_bin" init --global-skill < /dev/tty
else
    echo "Skipped. Run it later with: furmi init --global-skill"
fi
