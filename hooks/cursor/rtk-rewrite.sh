#!/usr/bin/env bash
# rtk-hook-version: 2
# RTK Cursor Agent hook — rewrites shell commands to use rtk for token savings.
# Works with both Cursor editor and cursor-cli (they share ~/.cursor/hooks.json).
# Cursor preToolUse hook format: receives JSON on stdin, returns JSON on stdout.
# Requires: rtk >= 0.23.0, jq
#
# This is a thin delegating hook: all rewrite logic lives in `rtk rewrite`,
# which is the single source of truth (src/discover/registry.rs).
# To add or change rewrite rules, edit the Rust registry — not this file.
#
# Exit code protocol for `rtk rewrite` (see src/hooks/rewrite_cmd.rs):
#   0 + stdout  Rewrite found, no deny/ask rule matched -> auto-allow
#   1           No RTK equivalent -> pass through unchanged
#   2           Deny rule matched -> pass through (Cursor deny handles it)
#   3 + stdout  Ask/Default rule matched -> rewrite but let Cursor confirm
#
# Issue #1272: a previous version of this script used `|| { echo '{}'; exit 0; }`
# which treated *any* non-zero exit from `rtk rewrite` as "no rewrite" - including
# the success-with-rewrite case (exit 3). This silently dropped every rewrite the
# agent issued. The current version inspects `$?` explicitly and emits the
# rewrite for both exit 0 and exit 3, only falling back to `{}` for exits 1/2/*.

if ! command -v jq &>/dev/null; then
  echo "[rtk] WARNING: jq is not installed. Hook cannot rewrite commands. Install jq: https://jqlang.github.io/jq/download/" >&2
  exit 0
fi

if ! command -v rtk &>/dev/null; then
  echo "[rtk] WARNING: rtk is not installed or not in PATH. Hook cannot rewrite commands. Install: https://github.com/rtk-ai/rtk#installation" >&2
  exit 0
fi

# Version guard: rtk rewrite was added in 0.23.0.
RTK_VERSION=$(rtk --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)
if [ -n "$RTK_VERSION" ]; then
  MAJOR=$(echo "$RTK_VERSION" | cut -d. -f1)
  MINOR=$(echo "$RTK_VERSION" | cut -d. -f2)
  if [ "$MAJOR" -eq 0 ] && [ "$MINOR" -lt 23 ]; then
    echo "[rtk] WARNING: rtk $RTK_VERSION is too old (need >= 0.23.0). Upgrade: cargo install rtk" >&2
    exit 0
  fi
fi

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

if [ -z "$CMD" ]; then
  echo '{}'
  exit 0
fi

# Delegate all rewrite + permission logic to the Rust binary.
# IMPORTANT: do NOT use `|| { echo '{}'; exit 0; }` here - `rtk rewrite` uses
# non-zero exit codes (3) to signal "rewrite + ask permission", not failure.
REWRITTEN=$(rtk rewrite "$CMD" 2>/dev/null)
EXIT_CODE=$?

case $EXIT_CODE in
  0|3)
    # 0 = rewrite + auto-allow, 3 = rewrite + ask.
    # Both produce a rewrite on stdout that we should apply. Cursor's
    # preToolUse panel currently enforces allow/deny only and can ignore
    # updated_input when permission is "ask", so use "allow" for both
    # exit codes - the underlying deny check (exit 2) is handled below.
    ;;
  *)
    # 1 = no RTK equivalent, 2 = deny rule matched, anything else = unknown.
    # In every case, return `{}` so Cursor proceeds with the original command.
    echo '{}'
    exit 0
    ;;
esac

# Safety net: empty stdout from `rtk rewrite` despite exit 0/3 means we have
# nothing to rewrite to - fall back to passthrough rather than emitting an
# empty `updated_input.command`.
if [ -z "$REWRITTEN" ]; then
  echo '{}'
  exit 0
fi

# No change - already an rtk command. Emit `{}` so Cursor doesn't see
# a no-op rewrite as a rewrite worth confirming.
if [ "$CMD" = "$REWRITTEN" ]; then
  echo '{}'
  exit 0
fi

jq -n --arg cmd "$REWRITTEN" '{
  "permission": "allow",
  "updated_input": { "command": $cmd }
}'
