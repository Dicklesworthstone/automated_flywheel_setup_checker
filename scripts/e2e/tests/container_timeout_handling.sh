#!/bin/bash
# E2E (Docker): a 60 s installer against a 5 s timeout is reported timedout and its container
# is removed.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "container_timeout_handling"
ensure_binary
require_docker
E2E_DOCKER_TIMEOUT=5 docker_fixture_config ""
add_docker_installer "slow_tool" <<'EOF' > /dev/null
#!/bin/bash
sleep 60
EOF

start=$(date +%s)
code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl" --timeout 5)
elapsed=$(( $(date +%s) - start ))
assert_eq "$code" "1" "timeout is a failure"
result=$(jsonl_result "$TEST_TMP/output/run.jsonl" slow_tool)
assert_eq "$(echo "$result" | jq -r .status)" "timedout" "status"
assert_eq "$(echo "$result" | jq -r .error.category)" "timeout" "category"
assert_lt "$elapsed" "55" "the sleeper was killed at the timeout (took ${elapsed}s)"
container=$(echo "$result" | jq -r '.container_id // empty')
if [[ -n "$container" ]]; then
    assert_eq "$(docker ps -a -q --filter "id=$container" | wc -l | tr -d ' ')" "0" "container removed"
fi
assert_eq "$(docker ps -a -q --filter label=afsc.managed=true --filter label=afsc.installer=slow_tool | wc -l | tr -d ' ')" "0" "no leaked container"
echo "container_timeout_handling: PASSED"
