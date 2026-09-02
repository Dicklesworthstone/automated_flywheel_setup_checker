#!/bin/bash
# E2E: a mixed batch (passes, a dependency failure, a checksum mismatch) run with --parallel 2,
# then `check --failed-from last` reruns exactly the failures.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "batch_run"
ensure_binary
add_pass "a_ok" > /dev/null
add_pass "b_ok" > /dev/null
add_pass "c_ok" > /dev/null
add_dependency_failure "d_dep" > /dev/null
add_installer_wrong_hash "e_bad_hash" <<'EOF' > /dev/null
#!/bin/bash
exit 0
EOF

code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl" --local --parallel 2)
assert_eq "$code" "1" "installer failures exit 1"
summary=$(jsonl_summary "$TEST_TMP/output/run.jsonl")
assert_eq "$(echo "$summary" | jq -r .total)" "5" "total"
assert_eq "$(echo "$summary" | jq -r .passed)" "3" "passed"
assert_eq "$(echo "$summary" | jq -r .failed)" "2" "failed"
assert_eq "$(jsonl_field "$TEST_TMP/output/run.jsonl" d_dep .error.category)" "dependency" "dependency category"
assert_eq "$(jsonl_field "$TEST_TMP/output/run.jsonl" e_bad_hash .error.category)" "checksum_mismatch" "mismatch category"

# Rerun only the failures.
code=$(run_check_jsonl "$TEST_TMP/output/rerun.jsonl" --local --failed-from last)
assert_eq "$code" "1" "still failing"
assert_eq "$(jq -c 'select(.kind == "result") | .installer_name' "$TEST_TMP/output/rerun.jsonl" | sort | tr -d '"' | paste -sd,)" "d_dep,e_bad_hash" "only the failures reran"
assert_eq "$(jq -r 'select(.kind == "run") | .installers_requested | length' "$TEST_TMP/output/rerun.jsonl")" "2" "header records the request"
echo "batch_run: PASSED"
