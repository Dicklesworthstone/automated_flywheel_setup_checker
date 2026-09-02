#!/bin/bash
# E2E: an unreachable HTTPS URL is classified as a transient network error and retried
# (retry_transient = 1 → 2 attempts recorded), checksum state unknown.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "network_failure"
ensure_binary
E2E_RETRIES=1 write_config
add_unreachable "offline_tool"

code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl" --local)
assert_eq "$code" "1" "network failure exits 1"
result=$(jsonl_result "$TEST_TMP/output/run.jsonl" offline_tool)
assert_eq "$(echo "$result" | jq -r .error.category)" "network" "category"
assert_eq "$(echo "$result" | jq -r .error.retryable)" "true" "retryable"
assert_eq "$(echo "$result" | jq -r '.attempts | length')" "2" "one retry recorded"
assert_eq "$(echo "$result" | jq -r .checksum_state)" "unknown" "nothing downloaded to verify"
assert_gt "$(echo "$result" | jq -r '.attempts[1].waited_before_ms')" "0" "backoff before the retry"
echo "network_failure: PASSED"
