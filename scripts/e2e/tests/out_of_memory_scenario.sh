#!/bin/bash
# E2E (Docker): an installer that allocates far more than the container memory limit is killed
# and reported as a failure with a resource classification (or the OOM exit code 137).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "out_of_memory_scenario"
ensure_binary
require_docker
docker_fixture_config "memory_limit = \"64m\""
add_docker_installer "hungry_tool" <<'EOF' > /dev/null
#!/bin/bash
python3 - <<'PY'
chunks = []
for _ in range(64):
    chunks.append(bytearray(16 * 1024 * 1024))
print("allocated", len(chunks) * 16, "MiB")
PY
EOF

code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl")
assert_eq "$code" "1" "OOM is a failure"
result=$(jsonl_result "$TEST_TMP/output/run.jsonl" hungry_tool)
assert_eq "$(echo "$result" | jq -r .status)" "failed" "status"
exit_code=$(echo "$result" | jq -r .exit_code)
category=$(echo "$result" | jq -r '.error.category')
assert_eq "$category" "resource" "SIGKILL/OOM is a resource failure (exit=$exit_code)"
assert_eq "$(echo "$result" | jq -r .error.retryable)" "false" "not retried"
assert_eq "$(docker ps -a -q --filter label=afsc.managed=true --filter label=afsc.installer=hungry_tool | wc -l | tr -d ' ')" "0" "no leaked container"
echo "out_of_memory_scenario: PASSED (exit=$exit_code category=$category)"
