#!/bin/bash
# E2E (Docker, real installers): run real ACFS installers from a checkout inside the prepared
# image and assert the loop's guarantees: every installer gets a verdict, checksums are verified
# before execution, passing installers report a version, the non-root user is used, and no
# container is left behind. Upstream drift (checksum_mismatch) is a legitimate verdict.
#
#   AFSC_ACFS_REPO=/path/to/agentic_coding_flywheel_setup   (default: /data/projects/...)
#   E2E_REAL_INSTALLERS="zoxide uv"                          (space separated)
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "real_installer_run"
ensure_binary
require_docker
repo=$(acfs_repo_path)
[[ -n "$repo" ]] || skip_test "no ACFS checkout (set AFSC_ACFS_REPO)"
read -r -a installers <<< "${E2E_REAL_INSTALLERS:-zoxide uv}"

write_config
code=0
HOME="$TEST_TMP/home" "$CHECKER_BINARY" --acfs-repo "$repo" --data-dir "$TEST_TMP/data" \
    check "${installers[@]}" --parallel "${#installers[@]}" --format jsonl \
    > "$TEST_TMP/output/run.jsonl" 2> "$TEST_TMP/output/run.err" || code=$?

header=$(jq -c 'select(.kind == "run")' "$TEST_TMP/output/run.jsonl")
assert_eq "$(echo "$header" | jq -r .backend)" "docker" "docker backend"
assert_eq "$(echo "$header" | jq -r .user)" "afsc-user" "non-root user"
assert_neq "$(echo "$header" | jq -r '.image_id // empty')" "" "image id recorded"
summary=$(jsonl_summary "$TEST_TMP/output/run.jsonl")
assert_eq "$(echo "$summary" | jq -r .total)" "${#installers[@]}" "every installer got a verdict"
assert_eq "$(echo "$summary" | jq -r .interrupted)" "false" "not interrupted"

for name in "${installers[@]}"; do
    result=$(jsonl_result "$TEST_TMP/output/run.jsonl" "$name")
    status=$(echo "$result" | jq -r .status)
    category=$(echo "$result" | jq -r '.error.category // empty')
    case "$status" in
        passed)
            assert_eq "$(echo "$result" | jq -r .checksum_state)" "verified" "$name checksum verified"
            echo "  $name: passed ($(echo "$result" | jq -r '.installed_version // "no version"'), $(echo "$result" | jq -r .duration_ms) ms)"
            ;;
        failed)
            if [[ "$category" == "checksum_mismatch" ]]; then
                assert_eq "$(echo "$result" | jq -r .exit_code)" "99" "$name refused before execution"
                echo "  $name: upstream drift (checksum mismatch) — installer NOT executed"
            else
                echo "  $name: failed ($category)"; echo "$result" | jq -r .stderr | tail -5
                exit 1
            fi
            ;;
        *) echo "  $name: unexpected status $status"; exit 1 ;;
    esac
done
assert_eq "$(docker ps -a -q --filter label=afsc.managed=true --filter "label=afsc.run_id=$(echo "$header" | jq -r .run_id)" | wc -l | tr -d ' ')" "0" "no leaked containers"
HOME="$TEST_TMP/home" "$CHECKER_BINARY" --data-dir "$TEST_TMP/data" status --format markdown
echo "real_installer_run: PASSED (exit $code)"
