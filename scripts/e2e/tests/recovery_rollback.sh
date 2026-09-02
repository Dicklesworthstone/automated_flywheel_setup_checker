#!/bin/bash
# E2E: a drifted pin fails, the pin is corrected, `check --failed-from last` passes, and
# `status --diff` reports the recovery; the history assessment sees both runs.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "recovery_rollback"
ensure_binary
add_pass "stable_tool" > /dev/null
path=$(add_installer_wrong_hash "drifted_tool" <<'EOF'
#!/bin/bash
echo "installed"
exit 0
EOF
)

code=$(run_check_jsonl "$TEST_TMP/output/run1.jsonl" --local)
assert_eq "$code" "1" "drift fails"
run1=$(jq -r 'select(.kind == "run") | .run_id' "$TEST_TMP/output/run1.jsonl")

# Correct the pin (what a checksum refresh would do) and rerun only the failure.
good=$(sha256_file "$path")
sed -i "s/0000000000000000000000000000000000000000000000000000000000000000/$good/" "$TEST_TMP/acfs/checksums.yaml"
code=$(run_check_jsonl "$TEST_TMP/output/run2.jsonl" --local --failed-from last)
assert_eq "$code" "0" "recovered"
assert_eq "$(jq -r 'select(.kind == "result") | .installer_name' "$TEST_TMP/output/run2.jsonl")" "drifted_tool" "only the failure reran"

diff=$(run_checker status --diff "${run1:0:8}" last --format json)
assert_eq "$(echo "$diff" | jq -r '.changes[] | select(.installer == "drifted_tool") | .change')" "recovered" "diff shows recovery"
hist=$(run_checker status --history drifted_tool --format json)
assert_eq "$(echo "$hist" | jq -r '.entries | length')" "2" "two history entries"
assert_eq "$(echo "$hist" | jq -r '.assessment.script_versions')" "2" "script hash changed between runs"
echo "recovery_rollback: PASSED"
