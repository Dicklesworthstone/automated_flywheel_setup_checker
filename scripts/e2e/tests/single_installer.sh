#!/bin/bash
# E2E: one mock installer, downloaded via file://, verified by SHA-256, executed with --local.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "single_installer"
ensure_binary
add_pass "good_tool" > /dev/null

code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl" --local)
assert_eq "$code" "0" "check exits 0 when every installer passes"

result=$(jsonl_result "$TEST_TMP/output/run.jsonl" good_tool)
assert_eq "$(echo "$result" | jq -r .status)" "passed" "status"
assert_eq "$(echo "$result" | jq -r .checksum_state)" "verified" "checksum verified before execution"
assert_eq "$(echo "$result" | jq -r .exit_code)" "0" "installer exit code"
assert_contains "$(echo "$result" | jq -r .stdout)" "mock installer ran as" "installer output captured"
assert_eq "$(jsonl_summary "$TEST_TMP/output/run.jsonl" | jq -r .passed)" "1" "summary passed"

# The run is persisted and visible through status.
assert_eq "$(run_checker status --format json | jq -r '.results[0].status')" "passed" "status shows the run"
assert_true "ls $(data_dir)/results/results_*.jsonl" "results file persisted"
echo "single_installer: PASSED"
