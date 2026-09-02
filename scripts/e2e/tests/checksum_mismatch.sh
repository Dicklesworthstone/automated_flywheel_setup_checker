#!/bin/bash
# E2E: a wrong pinned hash means the installer is never executed (exit 99, category
# checksum_mismatch, checksum_state mismatch).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "checksum_mismatch"
ensure_binary
marker="$TEST_TMP/output/executed.marker"
add_installer_wrong_hash "tampered" <<EOF > /dev/null
#!/bin/bash
touch "$marker"
exit 0
EOF

code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl" --local)
assert_eq "$code" "1" "mismatch is a failure"
assert_file_not_exists "$marker" "installer must not run"
result=$(jsonl_result "$TEST_TMP/output/run.jsonl" tampered)
assert_eq "$(echo "$result" | jq -r .status)" "failed" "status"
assert_eq "$(echo "$result" | jq -r .exit_code)" "99" "exit 99 from the verification wrapper"
assert_eq "$(echo "$result" | jq -r .checksum_state)" "mismatch" "checksum state"
assert_eq "$(echo "$result" | jq -r .error.category)" "checksum_mismatch" "category"
assert_eq "$(echo "$result" | jq -r .error.retryable)" "false" "not retryable"
assert_contains "$(echo "$result" | jq -r .stderr)" "CHECKSUM_MISMATCH" "stderr explains"
echo "checksum_mismatch: PASSED"
