#!/bin/bash
# E2E: the systemd install script renders units from the templates (dry run, nothing touched),
# every ExecStart parses with the real binary, and systemd-analyze verify accepts them when the
# binary is installed at the rendered path.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../lib/assertions.sh"
source "$SCRIPT_DIR/../lib/helpers.sh"

setup_test "systemd_integration"
ensure_binary
out="$TEST_TMP/output/units"
bash "$E2E_PROJECT_ROOT/scripts/install-systemd.sh" --dry-run --user svc --data-dir /srv/afsc --log-dir /srv/afsc/logs --out-dir "$out" > "$TEST_TMP/output/install.log"
assert_file_exists "$out/automated-flywheel-checker.service"
assert_file_exists "$out/automated-flywheel-checker-emergency.service"
assert_file_exists "$out/automated-flywheel-checker-serve.service"
assert_file_exists "$out/automated-flywheel-checker.timer"
assert_file_not_contains "$out/automated-flywheel-checker.service" "@" "no unrendered placeholders"
assert_file_contains "$out/automated-flywheel-checker.service" "ReadWritePaths=/srv/afsc /srv/afsc/logs"
assert_file_contains "$out/automated-flywheel-checker.service" "ExecStopPost=/usr/local/bin/automated_flywheel_setup_checker --config /etc/flywheel-checker/config.toml notify --last-run"

# Every ExecStart/ExecStopPost must parse with the real CLI (swap the binary path for ours).
for unit in "$out"/*.service; do
    while IFS= read -r line; do
        cmd="${line#Exec*=}"
        read -r -a argv <<< "$cmd"
        argv[0]="$CHECKER_BINARY"
        # --help short-circuits after parsing the leading arguments: exit 0 means they parse.
        "${argv[@]}" --help > /dev/null 2>&1 || { echo "does not parse: $line"; exit 1; }
    done < <(grep -E '^Exec(Start|StopPost)=' "$unit")
done

if command -v systemd-analyze > /dev/null 2>&1 && [[ -x /usr/local/bin/automated_flywheel_setup_checker ]]; then
    systemd-analyze verify "$out/automated-flywheel-checker.service" "$out/automated-flywheel-checker-serve.service"
else
    echo "note: systemd-analyze verify skipped (needs systemd and the installed binary path)"
fi
echo "systemd_integration: PASSED"
