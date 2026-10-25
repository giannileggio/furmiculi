#!/usr/bin/env bash
# Shared libraries for the browser tests, for a box where you can't install them.
#
# `npx playwright install --with-deps chromium` is the supported way, and it
# does this for you when you have root. Chromium's headless shell still needs
# the system libraries below, though, and a container (or a locked-down CI
# runner) has no root and often no apt lists. This fetches the same libraries as
# .deb packages, unpacks them into a directory of their own, and prints the
# LD_LIBRARY_PATH to put in front of `npm test`:
#
#   scripts/e2e-deps.sh            # into ${XDG_CACHE_HOME:-~/.cache}/furmi-e2e-libs
#   scripts/e2e-deps.sh DIR        # ...somewhere else
#
# Nothing is installed and nothing outside DIR is touched: no root, no apt lock,
# no packages of yours upgraded or removed. Pass --refresh to fetch again after
# a browser update; otherwise it checks the browser it can find, and does
# nothing when nothing is missing.
#
# Then, in e2e/:
#
#   LD_LIBRARY_PATH=$(scripts/e2e-deps.sh --print-only) npm test
set -euo pipefail

# The libraries the headless shell asks for on a bare Debian/Ubuntu-ish box,
# found with `ldd chrome-headless-shell | grep 'not found'`. Four of them grew a
# t64 suffix in Ubuntu 24.04; the download loop falls back to the older name.
PACKAGES=(
  libnspr4 libnss3
  libx11-6 libxcomposite1 libxdamage1 libxext6 libxfixes3 libxrandr2
  libxcb1 libxrender1 libxau6 libxdmcp6 libxi6 libdrm2 libgbm1 libxkbcommon0
  libasound2t64 libatk1.0-0t64 libatk-bridge2.0-0t64 libatspi2.0-0t64
)

print_only=0
refresh=0
dir=""
for arg in "$@"; do
  case "$arg" in
    --print-only) print_only=1 ;;
    --refresh) refresh=1 ;;
    -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown flag: $arg" >&2; exit 2 ;;
    *) dir=$arg ;;
  esac
done
if [ -z "$dir" ]; then
  dir="${XDG_CACHE_HOME:-$HOME/.cache}/furmi-e2e-libs"
fi

case "$(uname -s)" in
  Linux) ;;
  *) echo "this is for Linux; on macOS the browser needs nothing extra" >&2; exit 0 ;;
esac

if ! command -v dpkg-deb >/dev/null; then
  echo "dpkg-deb not found: this script unpacks .deb packages, so it is for Debian/Ubuntu-ish boxes" >&2
  exit 1
fi

# Where the browser the tests will run actually is, if Playwright has it: the
# newest one in its cache.
browser=$(find "${PLAYWRIGHT_BROWSERS_PATH:-$HOME/.cache/ms-playwright}" \
  -maxdepth 3 -type f -name chrome-headless-shell 2>/dev/null | sort | tail -1 || true)

# What it is still missing, given a library directory.
missing() {
  [ -n "$browser" ] || return 0
  LD_LIBRARY_PATH="${1:-}" ldd "$browser" 2>/dev/null | sed -n 's/.*not found$/\0/p' | wc -l | tr -d ' '
}

# Where a .deb puts shared libraries, e.g. usr/lib/x86_64-linux-gnu: the
# multiarch tuple, not `dpkg --print-architecture` (amd64).
multiarch=$(dpkg-architecture -qDEB_HOST_MULTIARCH 2>/dev/null || true)
[ -n "$multiarch" ] || multiarch="$(uname -m)-linux-gnu"
libdir="$dir/usr/lib/$multiarch"

if [ "$refresh" -eq 0 ] && [ -d "$libdir" ] && [ "$(missing "$libdir")" = "0" ]; then
  [ "$print_only" -eq 1 ] && { echo "$libdir"; exit 0; }
  echo "$libdir already has what the browser needs."
  echo "LD_LIBRARY_PATH=$libdir"
  exit 0
fi

if [ "$print_only" -eq 1 ]; then
  # Nothing to download and nothing checked: say what to put in the path, and
  # let the run find out.
  echo "$libdir"
  exit 0
fi

echo "Fetching the browser's system libraries into $dir"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# apt's own state lives under /var/lib/apt and needs root; give it scratch
# directories instead, which is enough to read the package lists and download.
apt_opts=(
  -o "Dir::State::Lists=$work/lists"
  -o "Dir::Cache=$work/cache"
  -o "Dir::Cache::archives=$work/downloads"
  -o "Dir::Cache::pkgcache=$work/cache/pkgcache.bin"
  -o "Dir::Cache::srcpkgcache=$work/cache/srcpkgcache.bin"
  -o "Debug::NoLocking=1"
  -o Dir::Etc::SourceList=/etc/apt/sources.list
  -o Dir::Etc::SourceParts=/etc/apt/sources.list.d
)
mkdir -p "$work/lists/partial" "$work/cache/archives/partial" "$work/downloads"
# Container images ship a hook that empties /var/cache/apt after every apt run,
# and that directory belongs to root here — the only thing it produces is a
# permission error, which is filtered out. Real failures are still printed.
if ! apt-get "${apt_opts[@]}" update -qq >"$work/apt.log" 2>&1; then
  cat "$work/apt.log" >&2
  exit 1
fi
grep -v "cannot remove '/var/cache/apt" "$work/apt.log" >&2 || true

(
  cd "$work/downloads"
  for package in "${PACKAGES[@]}"; do
    apt-get "${apt_opts[@]}" download "$package" >/dev/null 2>&1 ||
      apt-get "${apt_opts[@]}" download "${package%t64}" >/dev/null 2>&1 ||
      echo "warning: no package $package (or ${package%t64}) in this release's archive" >&2
  done
)

mkdir -p "$dir"
for deb in "$work"/downloads/*.deb; do
  [ -e "$deb" ] || continue
  dpkg-deb -x "$deb" "$dir"
done

if [ -z "$browser" ]; then
  echo "No Playwright browser found to check against: run 'npx playwright install chromium' in e2e/ first."
elif [ "$(missing "$libdir")" != "0" ]; then
  # Worth seeing rather than a mysterious browser crash later.
  echo "Still missing (a newer browser release may want more):" >&2
  LD_LIBRARY_PATH="$libdir" ldd "$browser" | sed -n 's/^\s*\(\S*\) => not found.*/  \1/p' >&2
  echo "LD_LIBRARY_PATH=$libdir"
  exit 1
fi

echo
echo "Done. Run the browser tests with:"
echo "  LD_LIBRARY_PATH=$libdir npm --prefix e2e test"
