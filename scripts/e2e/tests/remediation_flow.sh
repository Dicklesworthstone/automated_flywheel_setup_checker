#!/bin/bash
# E2E: `check --remediate` with a fake `claude` on PATH completes the run, keeps exit 1 for the
# failure, and records the remediation attempt in the metrics.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "remediation_flow"
ensure_binary
write_config "[remediation]
enabled = true
mode = \"advisory\"
cost_limit_usd = 1.0
timeout_seconds = 30
max_attempts = 1"
add_dependency_failure "broken_tool" > /dev/null

# Fake claude: accepts any arguments, prints a plausible envelope, never touches the repo.
mkdir -p "$TEST_TMP/bin"
cat > "$TEST_TMP/bin/claude" <<'EOF'
#!/bin/bash
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Advisory: apt package foo is missing; add apt-get install -y foo before use.\",\"total_cost_usd\":0.01}"
exit 0
EOF
chmod +x "$TEST_TMP/bin/claude"

code=0
PATH="$TEST_TMP/bin:$PATH" run_checker check --local --remediate --format jsonl > "$TEST_TMP/output/run.jsonl" 2> "$TEST_TMP/output/run.err" || code=$?
assert_eq "$code" "1" "the failure still fails the run"
assert_eq "$(jsonl_field "$TEST_TMP/output/run.jsonl" broken_tool .status)" "failed" "status"
assert_eq "$(jsonl_summary "$TEST_TMP/output/run.jsonl" | jq -r .failed)" "1" "summary"
# stdout stays pure JSONL even with remediation output.
assert_eq "$(grep -cv '^{' "$TEST_TMP/output/run.jsonl" || true)" "0" "stdout is JSONL only"
# The attempt is counted.
assert_contains "$(run_checker status --format prometheus)" "afsc_remediations_total_24h 1" "remediation counted"
echo "remediation_flow: PASSED"
