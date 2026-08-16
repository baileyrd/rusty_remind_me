#!/usr/bin/env bash
# Fail if `.claude-plugin/plugin.json`'s "version" has drifted from the
# workspace version in the root Cargo.toml.
#
# The two are independent, hand-maintained fields — nothing in Cargo itself
# ties a plugin manifest's version to a `[workspace.package]` version — so a
# PR that bumps one and forgets the other would otherwise ship a release
# whose attached plugin archive still claims the previous version. This is
# the schema-drift check's shape applied to that gap: catch it in CI, in
# seconds, rather than after a mismatched release is already published.
#
# Exit codes: 0 versions agree, 1 they differ, 2 the check could not run.
set -euo pipefail

# Two different roots since the rusty_recall merge, and conflating them is
# exactly how this check would silently stop checking anything. The plugin
# manifest is part of the rusty_remind_me half and stays beside it; the
# `[workspace.package]` version it is compared against moved up to the merged
# workspace root, one level further out. Before the merge these were the same
# directory.
remind_me_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd "$remind_me_root/.." && pwd)"
cargo_toml="$workspace_root/Cargo.toml"
plugin_json="$remind_me_root/.claude-plugin/plugin.json"

if [ ! -f "$cargo_toml" ]; then
  echo "error: $cargo_toml does not exist" >&2
  exit 2
fi

if [ ! -f "$plugin_json" ]; then
  echo "error: $plugin_json does not exist" >&2
  exit 2
fi

workspace_version="$("$remind_me_root/scripts/get_workspace_version.sh" "$cargo_toml")"

plugin_version="$(jq -r '.version' "$plugin_json")"
if [ -z "$plugin_version" ] || [ "$plugin_version" = "null" ]; then
  echo "error: $plugin_json has no string \"version\" field" >&2
  exit 2
fi

if [ "$workspace_version" != "$plugin_version" ]; then
  echo "Plugin version drift: Cargo.toml [workspace.package] is $workspace_version," \
    "but .claude-plugin/plugin.json is $plugin_version." >&2
  echo "Bump .claude-plugin/plugin.json's \"version\" to match in this PR." >&2
  exit 1
fi

echo "plugin.json version ($plugin_version) matches the workspace version."
