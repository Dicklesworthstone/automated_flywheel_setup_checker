#!/bin/bash
# E2E: failure categories from real runs (dependency, permission, network, resource) and the
# classify-error --explain command.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "error_classification"
ensure_binary
add_dependency_failure "dep_tool" > /dev/null
add_permission_failure "perm_tool" > /dev/null
add_unreachable "net_tool"
add_installer "disk_tool" <<'EOF' > /dev/null
#!/bin/bash
echo "tar: /opt/x: No space left on device" >&2
exit 1
EOF

code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl" --local --parallel 2)
assert_eq "$code" "1" "failures exit 1"
assert_eq "$(jsonl_field "$TEST_TMP/output/run.jsonl" dep_tool .error.category)" "dependency" "dependency"
assert_eq "$(jsonl_field "$TEST_TMP/output/run.jsonl" perm_tool .error.category)" "permission" "permission"
assert_eq "$(jsonl_field "$TEST_TMP/output/run.jsonl" net_tool .error.category)" "network" "network"
assert_eq "$(jsonl_field "$TEST_TMP/output/run.jsonl" disk_tool .error.category)" "resource" "resource"

explained=$(run_checker classify-error --stderr "E: Unable to locate package foo" --exit-code 100 --explain --format json)
assert_eq "$(echo "$explained" | jq -r .category)" "dependency" "classify-error category"
assert_contains "$(echo "$explained" | jq -r '.explain.pattern')" "unable to locate package" "explain names the pattern"
echo "error_classification: PASSED"
