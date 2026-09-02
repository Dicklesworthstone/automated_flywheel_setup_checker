#!/bin/bash
# E2E: stdout purity — with -vvv every stdout line is a JSON document with kind and
# schema_version; logs go to stderr only.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "jsonl_output"
ensure_binary
add_pass "good_tool" > /dev/null
add_dependency_failure "dep_tool" > /dev/null

code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl" --local -vvv)
assert_eq "$code" "1" "exit"
assert_eq "$(grep -cv '^{' "$TEST_TMP/output/run.jsonl" || true)" "0" "every stdout line is JSON"
assert_eq "$(jq -r '.kind' "$TEST_TMP/output/run.jsonl" | head -1)" "run" "first line is the run header"
assert_eq "$(jq -r '.kind' "$TEST_TMP/output/run.jsonl" | tail -1)" "summary" "last line is the summary"
assert_eq "$(jq -r 'select(.schema_version != 1) | .kind' "$TEST_TMP/output/run.jsonl" | wc -l | tr -d ' ')" "0" "schema_version on every line"
assert_eq "$(jq -r 'select(.kind == "result") | .installer_name' "$TEST_TMP/output/run.jsonl" | wc -l | tr -d ' ')" "2" "one result per installer"
assert_gt "$(wc -c < "$TEST_TMP/output/run.jsonl.err")" "0" "verbose logs went to stderr"
assert_contains "$(cat "$TEST_TMP/output/run.jsonl.err")" "DEBUG" "-vvv enables debug logs"

# JSON format: exactly one document.
run_checker check --local --format json > "$TEST_TMP/output/run.json" 2> /dev/null || true
assert_eq "$(jq -r .kind "$TEST_TMP/output/run.json")" "check" "single JSON document"
assert_eq "$(jq -r '.results | length' "$TEST_TMP/output/run.json")" "2" "results embedded"
echo "jsonl_output: PASSED"
