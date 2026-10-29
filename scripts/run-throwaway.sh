#!/usr/bin/env bash
# Build Rustmail and launch it with a throwaway profile, so real mail data is
# never touched.
#
#   scripts/run-throwaway.sh [--dev] [--no-build] [PROFILE_DIR]
#
#   --dev        use the dev profile (compiles faster, runs much slower)
#   --no-build   launch the existing binary without building
#   PROFILE_DIR  where the throwaway data/cache/config live
#                (default: ${TMPDIR:-/tmp}/rustmail-throwaway)
#
# CARGO_BUILD_JOBS defaults to 2 to keep memory use low; raise it on a big
# machine. Do not enter real credentials in the throwaway profile.
set -euo pipefail

profile=release
build=1
dir=""
for arg in "$@"; do
  case "$arg" in
    --dev) profile=dev ;;
    --no-build) build=0 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *) dir="$arg" ;;
  esac
done

export PATH="$HOME/.local/share/mise/shims:$HOME/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
cd "$(dirname "$0")/.."

dir="${dir:-${TMPDIR:-/tmp}/rustmail-throwaway}"
mkdir -p "$dir/data" "$dir/cache" "$dir/config"

if [ "$profile" = release ]; then
  [ "$build" = 1 ] && cargo build --release
  bin=target/release/rustmail
else
  [ "$build" = 1 ] && cargo build
  bin=target/debug/rustmail
fi

[ -x "$bin" ] || { echo "$bin not found; run without --no-build" >&2; exit 1; }

echo "Throwaway profile: $dir" >&2
XDG_DATA_HOME="$dir/data" XDG_CACHE_HOME="$dir/cache" XDG_CONFIG_HOME="$dir/config" \
  exec "$bin"
