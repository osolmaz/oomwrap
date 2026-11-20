#!/usr/bin/env bash
# Builds oomwrap and the real-system suite, and copies both binaries into the
# folder $1 as `oomwrap` and `real_system`. Further arguments go to cargo,
# such as --release.
set -euo pipefail

dist="$1"
shift
mkdir -p "$dist"

executable() {
  jq -r --arg name "$1" 'select(.target.name == $name and .executable != null) | .executable'
}

oomwrap="$(cargo build "$@" --locked --bin oomwrap --message-format=json | executable oomwrap)"
suite="$(cargo test "$@" --locked --test real_system --no-run --message-format=json | executable real_system)"
cp "$oomwrap" "$dist/oomwrap"
cp "$suite" "$dist/real_system"
