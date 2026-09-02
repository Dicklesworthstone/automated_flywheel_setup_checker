#!/bin/bash
# E2E: three 2-second installers finish in about 2 s with --parallel 3 and about 6 s sequentially.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "parallel_execution"
ensure_binary
add_sleeper "sleep_a" 2 > /dev/null
add_sleeper "sleep_b" 2 > /dev/null
add_sleeper "sleep_c" 2 > /dev/null

start=$(date +%s)
code=$(run_check_jsonl "$TEST_TMP/output/par.jsonl" --local --parallel 3)
par=$(( $(date +%s) - start ))
assert_eq "$code" "0" "parallel run passes"
assert_eq "$(jq -r 'select(.kind == "run") | .parallel' "$TEST_TMP/output/par.jsonl")" "3" "header records parallelism"
assert_lt "$par" "6" "parallel wall time ($par s) is below sequential"

start=$(date +%s)
code=$(run_check_jsonl "$TEST_TMP/output/seq.jsonl" --local --parallel 1)
seq=$(( $(date +%s) - start ))
assert_eq "$code" "0" "sequential run passes"
assert_gt "$seq" "5" "sequential wall time ($seq s) is the sum of the sleeps"
assert_eq "$(jsonl_summary "$TEST_TMP/output/seq.jsonl" | jq -r .passed)" "3" "all passed"
echo "parallel_execution: PASSED (parallel ${par}s, sequential ${seq}s)"
