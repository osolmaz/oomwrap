#!/usr/bin/env bash
# Prints the version of the oomwrap package.
set -euo pipefail

cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name == "oomwrap") | .version'
