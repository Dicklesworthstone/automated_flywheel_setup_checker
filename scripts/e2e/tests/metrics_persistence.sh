#!/bin/bash
# ============================================================
# E2E Test: Metrics Persistence
#
# Validates that repeated real `check --local` runs persist and
# increment the metrics snapshot at ~/.local/share/afsc/metrics.json.
# ============================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "metrics_persistence"

echo "Test: Metrics persistence wiring"

ensure_binary

HOME_DIR="$TEST_TMP/home"
ACFS_REPO="$TEST_TMP/acfs"
mkdir -p "$HOME_DIR" "$ACFS_REPO"

create_fixture "metrics_installer.sh" << 'INSTALL'
#!/bin/bash
echo "installer ran"
exit 0
INSTALL

INSTALLER_PATH="$TEST_TMP/fixtures/metrics_installer.sh"
INSTALLER_SHA="$(sha256_file "$INSTALLER_PATH")"

cat > "$ACFS_REPO/checksums.yaml" << EOF
installers:
  metrics-tool:
    url: "file://$INSTALLER_PATH"
    sha256: "$INSTALLER_SHA"
    enabled: true
EOF

cat > "$TEST_TMP/config.toml" << EOF
[general]
acfs_repo = "$ACFS_REPO"
log_level = "info"
allow_file_urls = true

[docker]
image = "ubuntu:22.04"
memory_limit = "2G"
cpu_quota = 1.0
timeout_seconds = 300
pull_policy = "if-not-present"

[execution]
parallel = 1
retry_transient = 3
fail_fast = false

[remediation]
enabled = false
auto_commit = false
create_pr = true
max_attempts = 3
EOF

CONFIG_PATH="$TEST_TMP/config.toml"
METRICS_PATH="$HOME_DIR/.local/share/afsc/metrics.json"

run_check() {
    HOME="$HOME_DIR" "$CHECKER_BINARY" --config "$CONFIG_PATH" check --local --yes
}

echo "  [1/2] Running first local check..."
first_output="$(run_check 2>&1)"
assert_contains "$first_output" "Results: 1 passed, 0 failed out of 1 total" \
    "First check should report one successful installer"
assert_file_exists "$METRICS_PATH" "Metrics snapshot should be created after first run"
assert_json_field "$METRICS_PATH" ".total_tests_24h" "1" \
    "First run should persist one total test"
assert_json_field "$METRICS_PATH" ".successful_tests_24h" "1" \
    "First run should persist one successful test"
assert_json_field "$METRICS_PATH" ".total_remediations_24h" "0" \
    "First run should not record remediations"
assert_matches "$(jq -r '.success_rate_24h' "$METRICS_PATH")" '^1(\.0+)?$' \
    "First run should record a perfect success rate"
assert_json_exists "$METRICS_PATH" ".last_test" "Metrics snapshot should record last_test"
assert_json_exists "$METRICS_PATH" ".last_success" \
    "Metrics snapshot should record last_success"
assert_json_not_exists "$METRICS_PATH" ".last_failure" \
    "Successful runs should not set last_failure"

echo "  [2/2] Running second local check..."
second_output="$(run_check 2>&1)"
assert_contains "$second_output" "Results: 1 passed, 0 failed out of 1 total" \
    "Second check should also report one successful installer"
assert_json_field "$METRICS_PATH" ".total_tests_24h" "2" \
    "Second run should increment total test count"
assert_json_field "$METRICS_PATH" ".successful_tests_24h" "2" \
    "Second run should increment successful test count"
assert_json_field "$METRICS_PATH" ".total_remediations_24h" "0" \
    "Second run should still have zero remediations"
assert_matches "$(jq -r '.success_rate_24h' "$METRICS_PATH")" '^1(\.0+)?$' \
    "Second run should preserve a perfect success rate"

echo "Metrics persistence test: PASSED"
cleanup_test
