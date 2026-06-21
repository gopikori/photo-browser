#!/usr/bin/env bash
# Start photo-browser. Builds the release binary on first run, then launches it.
# Usage:
#   ./start.sh                 # opens the in-app folder picker (default: ~/Downloads)
#   ./start.sh /path/to/folder # opens that folder directly (sorts it first)
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$DIR/target/release/photo-browser"

# Build if the binary is missing or any source is newer than it.
needs_build=0
if [[ ! -x "$BIN" ]]; then
  needs_build=1
else
  while IFS= read -r src; do
    [[ "$src" -nt "$BIN" ]] && { needs_build=1; break; }
  done < <(find "$DIR/src" "$DIR/Cargo.toml" -type f)
fi

if [[ "$needs_build" -eq 1 ]]; then
  echo "Building photo-browser (release)…"
  ( cd "$DIR" && cargo build --release )
fi

exec "$BIN" "$@"
