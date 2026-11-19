#!/usr/bin/env bash
# Prints the Cargo workspace version ([workspace.package] in Cargo.toml), the version of every Zora release.
set -euo pipefail
cd "$(dirname "$0")/../.."
v=$(awk '/^\[workspace\.package\]/ { f = 1; next } /^\[/ { f = 0 }
         f && /^version *=/ { sub(/^version *= *"/, ""); sub(/".*/, ""); print; exit }' Cargo.toml)
if [[ ! "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Cargo.toml [workspace.package] has no X.Y.Z version (got '$v')." >&2
  exit 1
fi
echo "$v"
