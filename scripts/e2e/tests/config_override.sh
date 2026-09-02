#!/bin/bash
# E2E: precedence defaults < file < AFSC_* env < CLI, visible through `config show --resolved`
# and the run header; unknown keys fail `config validate --strict`.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "config_override"
ensure_binary
add_pass "good_tool" > /dev/null
E2E_PARALLEL=2 write_config

resolved=$(run_checker config show --resolved --format json)
assert_eq "$(echo "$resolved" | jq -r '.config.execution.parallel')" "2" "file value"
assert_eq "$(echo "$resolved" | jq -r '.sources["execution.parallel"]')" "file" "file source"

resolved=$(AFSC_EXECUTION_PARALLEL=3 run_checker config show --resolved --format json)
assert_eq "$(echo "$resolved" | jq -r '.config.execution.parallel')" "3" "env overrides file"
assert_eq "$(echo "$resolved" | jq -r '.sources["execution.parallel"]')" "env" "env source"

header=$(AFSC_EXECUTION_PARALLEL=3 run_checker check --local --dry-run --parallel 4 --format json 2> /dev/null)
assert_eq "$(echo "$header" | jq -r .parallel)" "4" "CLI overrides env"
assert_eq "$(echo "$header" | jq -r .dry_run)" "true" "dry run"

write_config "[dokcer]
image = \"typo\""
code=0
run_checker config validate --strict > /dev/null 2> "$TEST_TMP/output/strict.err" || code=$?
assert_eq "$code" "2" "unknown section is a config error under --strict"
assert_contains "$(cat "$TEST_TMP/output/strict.err")" "dokcer" "names the unknown key"
echo "config_override: PASSED"
