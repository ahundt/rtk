#!/usr/bin/env bash
# Test suite for hooks/cursor/rtk-rewrite.sh
# Feeds mock Cursor preToolUse JSON through the hook and verifies the
# rewritten commands and exit-code handling.
#
# Issue #1272 regression coverage: prior versions of the cursor hook used
# `REWRITTEN=$(rtk rewrite ...) || { echo '{}'; exit 0; }`, which discarded
# every successful rewrite because `rtk rewrite` returns exit 3 (rewrite +
# ask) on success — non-zero exit codes were misread as failure.
#
# Usage:
#   bash hooks/cursor/test-rtk-rewrite.sh
# Or from CI:
#   PATH="$PWD/target/debug:$PATH" bash hooks/cursor/test-rtk-rewrite.sh

set -u

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOOK="${HOOK:-$HOOK_DIR/rtk-rewrite.sh}"
PASS=0
FAIL=0
TOTAL=0

GREEN=$'\033[32m'
RED=$'\033[31m'
DIM=$'\033[2m'
RESET=$'\033[0m'

if ! command -v jq &>/dev/null; then
  echo "[skip] jq not installed — cannot run hook tests"
  exit 0
fi

if ! command -v rtk &>/dev/null; then
  echo "[skip] rtk not on PATH — cannot run hook tests"
  exit 0
fi

# Helper: feed input JSON through the cursor hook and capture stdout.
run_hook() {
  local input="$1"
  echo "$input" | bash "$HOOK" 2>/dev/null
}

# Assertion: the hook output, when piped through jq, has
# `.updated_input.command == "$expected"` and `.permission == "allow"`.
assert_rewrite() {
  local description="$1"
  local input_cmd="$2"
  local expected_cmd="$3"
  TOTAL=$((TOTAL + 1))

  local input_json output actual permission
  input_json=$(jq -n --arg cmd "$input_cmd" '{"tool_input":{"command":$cmd}}')
  output=$(run_hook "$input_json")
  actual=$(echo "$output" | jq -r '.updated_input.command // empty' 2>/dev/null)
  permission=$(echo "$output" | jq -r '.permission // empty' 2>/dev/null)

  if [ "$actual" = "$expected_cmd" ] && [ "$permission" = "allow" ]; then
    printf "  %sPASS%s %s %s-> %s%s\n" "$GREEN" "$RESET" "$description" "$DIM" "$actual" "$RESET"
    PASS=$((PASS + 1))
  else
    printf "  %sFAIL%s %s\n" "$RED" "$RESET" "$description"
    printf "       expected command:    %s\n" "$expected_cmd"
    printf "       actual command:      %s\n" "$actual"
    printf "       expected permission: allow\n"
    printf "       actual permission:   %s\n" "$permission"
    printf "       raw output:          %s\n" "$output"
    FAIL=$((FAIL + 1))
  fi
}

# Assertion: hook output is exactly `{}` (literal empty-object JSON), i.e.
# the hook decided to pass the original command through unchanged.
assert_passthrough() {
  local description="$1"
  local input_cmd="$2"
  TOTAL=$((TOTAL + 1))

  local input_json output
  input_json=$(jq -n --arg cmd "$input_cmd" '{"tool_input":{"command":$cmd}}')
  output=$(run_hook "$input_json")

  if [ "$output" = "{}" ]; then
    printf "  %sPASS%s %s %s-> (passthrough)%s\n" "$GREEN" "$RESET" "$description" "$DIM" "$RESET"
    PASS=$((PASS + 1))
  else
    printf "  %sFAIL%s %s\n" "$RED" "$RESET" "$description"
    printf "       expected: {}\n"
    printf "       actual:   %s\n" "$output"
    FAIL=$((FAIL + 1))
  fi
}

echo "============================================"
echo "  RTK Cursor Rewrite Hook Test Suite"
echo "  HOOK = $HOOK"
echo "============================================"
echo ""
echo "--- Issue #1272: rewrites must survive exit code 3 ---"

# Each of these is rewritten by `rtk rewrite` (which exits 0 OR 3 with a
# rewrite on stdout). The hook MUST emit the rewrite as `updated_input`.
assert_rewrite "git status" "git status" "rtk git status"
assert_rewrite "ls -la" "ls -la" "rtk ls -la"
assert_rewrite "git diff HEAD" "git diff HEAD" "rtk git diff HEAD"
assert_rewrite "cargo test" "cargo test" "rtk cargo test"

echo ""
echo "--- Passthrough cases (no rewrite available) ---"

# `rtk rewrite` exits 1 for these — hook must emit `{}`.
assert_passthrough "htop (no RTK equivalent)" "htop"
assert_passthrough "vim file.txt (editor)" "vim file.txt"

echo ""
echo "--- Edge cases ---"

# Empty/missing command: hook must still emit `{}` (valid JSON).
empty_input='{"tool_input":{"command":""}}'
TOTAL=$((TOTAL + 1))
if [ "$(echo "$empty_input" | bash "$HOOK" 2>/dev/null)" = "{}" ]; then
  printf "  %sPASS%s empty command -> {}\n" "$GREEN" "$RESET"
  PASS=$((PASS + 1))
else
  printf "  %sFAIL%s empty command did not return {}\n" "$RED" "$RESET"
  FAIL=$((FAIL + 1))
fi

# Already-rtk command: no rewrite needed -> `{}`.
assert_passthrough "rtk git status (already rtk)" "rtk git status"

echo ""
echo "============================================"
printf "  Results: %s/%s passed, %s failed\n" "$PASS" "$TOTAL" "$FAIL"
echo "============================================"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
exit 0
