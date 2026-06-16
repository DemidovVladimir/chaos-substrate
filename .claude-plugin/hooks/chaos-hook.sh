#!/usr/bin/env sh
# Chaos Substrate plugin hook launcher.
#
# Claude Code invokes this on Bash|Grep|Glob tool calls so the `chaos
# hook` subcommand can inject code-memory context. The launcher MUST never break
# the host tool call: it self-locates the chaos binary wherever it actually
# lives and degrades to a SILENT no-op (exit 0, nothing on stdout/stderr) when
# the binary, config, or database is unavailable — instead of the shell erroring
# with "No such file or directory" on every single tool call, which is what
# happens when a hook hard-codes a build-artifact path that a fresh, marketplace,
# or zip install does not contain.
#
# Usage: chaos-hook.sh <PreToolUse|PostToolUse> [extra args forwarded to `chaos hook`...]
set -u

event="${1:-PreToolUse}"
[ "$#" -gt 0 ] && shift

# Resolve this script's own directory, following symlinks, without `readlink -f`
# (not available on stock macOS). Everything else is derived from here, so the
# launcher does not depend on how the host sets CLAUDE_PLUGIN_ROOT.
src="$0"
while [ -L "$src" ]; do
  dir="$(cd -P "$(dirname "$src")" 2>/dev/null && pwd)" || exit 0
  link="$(readlink "$src")"
  case "$link" in
    /*) src="$link" ;;
    *)  src="$dir/$link" ;;
  esac
done
hook_dir="$(cd -P "$(dirname "$src")" 2>/dev/null && pwd)" || exit 0
# .../.claude-plugin/hooks  ->  checkout root (target/, *.toml, bin/ live here)
repo_root="$(cd -P "$hook_dir/../.." 2>/dev/null && pwd)" || exit 0

# Resolve the chaos binary: explicit override -> checkout build -> PATH wrapper.
# Bail out as a no-op if none exists, so a binary-less install never spams.
bin="${CHAOS_BIN:-}"
if [ -z "$bin" ] || [ ! -x "$bin" ]; then
  if [ -x "$repo_root/target/release/chaos" ]; then
    bin="$repo_root/target/release/chaos"
  elif command -v chaos >/dev/null 2>&1; then
    bin="$(command -v chaos)"
  else
    exit 0
  fi
fi

# Resolve a config if one is present; otherwise let the binary fall back to its
# own defaults / the DATABASE_URL env (the hook is read-only and embedder-free).
config="${CHAOS_CONFIG:-}"
if [ -z "$config" ] || [ ! -f "$config" ]; then
  if [ -f "$repo_root/chaos-substrate.toml" ]; then
    config="$repo_root/chaos-substrate.toml"
  elif [ -f "$repo_root/chaos-substrate.local.toml" ]; then
    config="$repo_root/chaos-substrate.local.toml"
  else
    config=""
  fi
fi

if [ -n "$config" ]; then
  exec "$bin" --config "$config" hook --event "$event" "$@"
else
  exec "$bin" hook --event "$event" "$@"
fi
