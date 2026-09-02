#!/bin/bash
# E2E: "No space left on device" (produced by writing to /dev/full, no real disk exhaustion
# needed) is classified as a resource failure and surfaced in status --detailed.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "disk_space_exhaustion"
ensure_binary
add_installer "full_disk_tool" <<'EOF' > /dev/null
#!/bin/bash
echo "extracting toolchain..."
if ! echo "payload" > /dev/full 2> /tmp/write.err; then
    echo "tar: ./toolchain/bin/rustc: $(cat /tmp/write.err | sed 's/.*: //')" >&2
    exit 2
fi
EOF

code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl" --local)
assert_eq "$code" "1" "failure"
result=$(jsonl_result "$TEST_TMP/output/run.jsonl" full_disk_tool)
assert_eq "$(echo "$result" | jq -r .error.category)" "resource" "resource category"
assert_eq "$(echo "$result" | jq -r .error.retryable)" "false" "not retried"
assert_contains "$(run_checker status --detailed)" "error: resource" "status --detailed shows the category"
echo "disk_space_exhaustion: PASSED"
