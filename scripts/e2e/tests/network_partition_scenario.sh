#!/bin/bash
# E2E (Docker): with [docker].network = "none" an installer that reaches for the internet fails
# with a network classification; the mock script itself is served from the bind mount.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "network_partition_scenario"
ensure_binary
require_docker
docker_fixture_config "network = \"none\""
add_docker_installer "online_tool" <<'EOF' > /dev/null
#!/bin/bash
curl -fsSL --max-time 10 https://example.com/ -o /tmp/page.html
EOF

code=$(run_check_jsonl "$TEST_TMP/output/run.jsonl")
assert_eq "$code" "1" "partition is a failure"
result=$(jsonl_result "$TEST_TMP/output/run.jsonl" online_tool)
assert_eq "$(echo "$result" | jq -r .status)" "failed" "status"
assert_eq "$(echo "$result" | jq -r .error.category)" "network" "network category"
assert_eq "$(echo "$result" | jq -r .checksum_state)" "verified" "the script itself came from the mount"
echo "network_partition_scenario: PASSED"
