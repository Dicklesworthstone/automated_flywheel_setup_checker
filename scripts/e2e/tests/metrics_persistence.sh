#!/bin/bash
# E2E: metrics are derived from persisted runs — metrics.json, `status --format prometheus`
# with per-installer gauges, and `validate --check-hashes` drift exposure.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "metrics_persistence"
ensure_binary
add_pass "good_tool" > /dev/null
add_dependency_failure "dep_tool" > /dev/null

assert_contains "$(run_checker status --format prometheus)" "afsc_health -1" "no data before any run"
code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl" --local)
assert_eq "$code" "1" "exit"

assert_file_exists "$(data_dir)/metrics.json"
assert_eq "$(jq -r .total_tests_24h "$(data_dir)/metrics.json")" "2" "snapshot counts tests"
assert_eq "$(jq -r .successful_tests_24h "$(data_dir)/metrics.json")" "1" "snapshot counts passes"

prom=$(run_checker status --format prometheus)
assert_contains "$prom" "afsc_tests_total_24h 2" "rolling total"
assert_contains "$prom" "afsc_runs_24h 1" "runs"
assert_contains "$prom" "afsc_health 1" "healthy"
assert_contains "$prom" 'afsc_installer_status{installer="dep_tool"} 0' "per-installer failure"
assert_contains "$prom" 'afsc_installer_status{installer="good_tool"} 1' "per-installer pass"
assert_contains "$prom" "afsc_run_last_timestamp " "last run timestamp"

# Drift from validate is exposed once a hash check ran.
add_installer_wrong_hash "drifted" <<'EOF' > /dev/null
#!/bin/bash
exit 0
EOF
code=0
run_checker validate --check-hashes > /dev/null 2>&1 || code=$?
assert_eq "$code" "4" "validate exits 4 on drift"
assert_file_exists "$(data_dir)/validate.json"
assert_contains "$(run_checker status --format prometheus)" "afsc_checksum_drift_total 1" "drift gauge"
echo "metrics_persistence: PASSED"
