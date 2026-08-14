#!/usr/bin/env bash
# Print this workspace's version — the `version = "..."` line inside
# `[workspace.package]` in the root Cargo.toml — to stdout, or exit non-zero
# with nothing printed if it can't be found unambiguously.
#
# Used by the release workflow to name the tag/release, and by
# check_plugin_version.sh to compare against `.claude-plugin/plugin.json`.
#
# Deliberately restricted to the `[workspace.package]` table rather than a
# bare `grep -m1 '^version ='` over the whole file: every workspace member's
# own Cargo.toml has a `version.workspace = true` line instead of a literal
# version, so a naive first-match grep happens to be safe today, but is one
# added `[package]` block with a hand-set `version = "..."` away from
# silently picking up the wrong value.
#
# Usage: scripts/get_workspace_version.sh [path/to/Cargo.toml]
set -euo pipefail

cargo_toml="${1:-Cargo.toml}"

start_line="$(grep -n '^\[workspace\.package\]' "$cargo_toml" | head -1 | cut -d: -f1)"
if [ -z "$start_line" ]; then
  echo "error: no [workspace.package] table in $cargo_toml" >&2
  exit 2
fi

version_line="$(awk -v start="$start_line" '
  NR > start && /^\[/ { exit }
  NR > start && /^version[ \t]*=/ { print; exit }
' "$cargo_toml")"

if [ -z "$version_line" ]; then
  echo "error: no version = \"...\" line found in [workspace.package] of $cargo_toml" >&2
  exit 2
fi

version="$(echo "$version_line" | sed -E 's/^version[ \t]*=[ \t]*"([^"]*)".*/\1/')"
if [ -z "$version" ] || [ "$version" = "$version_line" ]; then
  echo "error: could not parse a quoted version out of: $version_line" >&2
  exit 2
fi

echo "$version"
