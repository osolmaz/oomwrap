#!/usr/bin/env bash
# Fails unless each file is a static executable: no program interpreter and
# no shared library that it needs.
set -euo pipefail

for file in "$@"; do
  if readelf --program-headers --wide "$file" | grep -q 'INTERP'; then
    echo "::error::$file needs a program interpreter"
    exit 1
  fi
  if readelf --dynamic --wide "$file" | grep -q '(NEEDED)'; then
    echo "::error::$file needs shared libraries"
    readelf --dynamic --wide "$file" | grep '(NEEDED)'
    exit 1
  fi
  echo "$file is static"
done
