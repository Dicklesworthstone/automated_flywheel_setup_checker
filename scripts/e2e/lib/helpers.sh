#!/bin/bash
# ============================================================
# E2E helper library: every test drives the real binary against a synthetic ACFS repo
# (checksums.yaml + mock installer scripts served via file://) in an isolated HOME.
#
# Layout per test (under $TEST_TMP):
#   acfs/checksums.yaml   installers: {name: {url, sha256}}
#   fixtures/<name>.sh     mock installer scripts
#   home/                  HOME for the checker (data dir: home/.local/share/afsc)
#   config.toml            [general] acfs_repo, allow_file_urls, [execution] ...
# ============================================================

E2E_PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
E2E_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

TEST_TMP=""
TEST_NAME=""
CHECKER_BINARY="${CHECKER_BINARY:-${CARGO_TARGET_DIR:-${E2E_PROJECT_ROOT}/target}/release/automated_flywheel_setup_checker}"

_helper_log() {
    echo -e "\033[90m[HELPER]\033[0m $*"
}

# setup_test "name" — fresh temp tree, empty checksums.yaml, default config, cleanup trap.
setup_test() {
    TEST_NAME="$1"
    TEST_TMP=$(mktemp -d "${TMPDIR:-/tmp}/e2e-${TEST_NAME}-XXXXXX")
    _helper_log "Setting up test: $TEST_NAME in $TEST_TMP"
    mkdir -p "$TEST_TMP/acfs" "$TEST_TMP/fixtures" "$TEST_TMP/home" "$TEST_TMP/output"
    printf 'installers:\n' > "$TEST_TMP/acfs/checksums.yaml"
    write_config
    trap cleanup_test EXIT
}

cleanup_test() {
    if [[ -n "$TEST_TMP" && -d "$TEST_TMP" && "${E2E_KEEP_TMP:-0}" != "1" ]]; then
        _helper_log "Cleaning up: $TEST_TMP"
        rm -rf "$TEST_TMP"
    fi
}

# write_config [extra toml...] — (re)write config.toml; each extra argument is appended verbatim.
write_config() {
    {
        echo "[general]"
        echo "acfs_repo = \"$TEST_TMP/acfs\""
        echo "log_level = \"info\""
        echo "allow_file_urls = true"
        echo ""
        echo "[execution]"
        echo "parallel = ${E2E_PARALLEL:-1}"
        echo "retry_transient = ${E2E_RETRIES:-0}"
        echo "fail_fast = ${E2E_FAIL_FAST:-false}"
        echo ""
        for extra in "$@"; do
            printf '%s\n' "$extra"
            echo ""
        done
    } > "$TEST_TMP/config.toml"
}

sha256_file() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# add_entry name url sha — append an installer to checksums.yaml
add_entry() {
    local name="$1" url="$2" sha="$3"
    printf '  %s:\n    url: "%s"\n    sha256: "%s"\n' "$name" "$url" "$sha" >> "$TEST_TMP/acfs/checksums.yaml"
}

# add_installer name — reads the script body from stdin, pins its real hash, returns the path
add_installer() {
    local name="$1"
    local path="$TEST_TMP/fixtures/$name.sh"
    cat > "$path"
    chmod +x "$path"
    add_entry "$name" "file://$path" "$(sha256_file "$path")"
    echo "$path"
}

# add_installer_wrong_hash name — same, but pins a wrong hash (the checker must refuse to run it)
add_installer_wrong_hash() {
    local name="$1"
    local path="$TEST_TMP/fixtures/$name.sh"
    cat > "$path"
    chmod +x "$path"
    add_entry "$name" "file://$path" "0000000000000000000000000000000000000000000000000000000000000000"
    echo "$path"
}

# Common fixtures
add_pass() { add_installer "$1" <<'EOF'
#!/bin/bash
echo "mock installer ran as $(id -un) HOME=$HOME"
exit 0
EOF
}
add_dependency_failure() { add_installer "$1" <<'EOF'
#!/bin/bash
echo "E: Unable to locate package foo" >&2
exit 100
EOF
}
add_permission_failure() { add_installer "$1" <<'EOF'
#!/bin/bash
echo "Permission denied" >&2
exit 126
EOF
}
add_sleeper() { # name seconds
    local name="$1" secs="$2"
    add_installer "$name" <<EOF
#!/bin/bash
sleep $secs
exit 0
EOF
}
add_unreachable() {
    add_entry "$1" "https://127.0.0.1:9/nope.sh" "1111111111111111111111111111111111111111111111111111111111111111"
}

# run_checker [args...] — run the binary with the test HOME/config; local runs pre-approved.
run_checker() {
    if [[ ! -x "$CHECKER_BINARY" ]]; then
        echo "ERROR: binary not found at $CHECKER_BINARY (cargo build --release)" >&2
        return 1
    fi
    HOME="$TEST_TMP/home" AFSC_ALLOW_LOCAL=1 "$CHECKER_BINARY" --config "$TEST_TMP/config.toml" "$@"
}

# run_check_jsonl OUTFILE [args...] — `check --format jsonl` capturing stdout to OUTFILE,
# stderr to OUTFILE.err; echoes the exit code (never aborts the test).
run_check_jsonl() {
    local out="$1"; shift
    local code=0
    run_checker check --format jsonl "$@" > "$out" 2> "$out.err" || code=$?
    echo "$code"
}

# jsonl_result FILE installer — the result line for an installer (JSON object)
jsonl_result() {
    jq -c --arg n "$2" 'select(.kind == "result" and .installer_name == $n)' "$1"
}

# jsonl_summary FILE — the summary line
jsonl_summary() {
    jq -c 'select(.kind == "summary")' "$1"
}

# jsonl_field FILE installer .path — a field of an installer's result
jsonl_field() {
    jsonl_result "$1" "$2" | jq -r "$3"
}

# data_dir — where results/metrics/logs land for this test
data_dir() {
    echo "$TEST_TMP/home/.local/share/afsc"
}

wait_for() {
    local condition="$1" timeout="${2:-30}" interval="${3:-1}" elapsed=0
    while [[ $elapsed -lt $timeout ]]; do
        if eval "$condition" > /dev/null 2>&1; then
            return 0
        fi
        sleep "$interval"
        elapsed=$((elapsed + interval))
    done
    return 1
}

# require_docker — skip (exit 0 with SKIP marker) when no daemon is reachable
require_docker() {
    if ! docker info > /dev/null 2>&1; then
        echo "SKIP: Docker not available"
        exit 0
    fi
}

skip_test() {
    echo "SKIP: $1"
    exit 0
}

ensure_binary() {
    if [[ ! -x "$CHECKER_BINARY" ]]; then
        _helper_log "Binary not found, building..."
        cargo build --release --manifest-path "$E2E_PROJECT_ROOT/Cargo.toml" 2>&1 || {
            echo "ERROR: Failed to build binary" >&2
            return 1
        }
    fi
}

# acfs_repo_path — a real ACFS checkout for real-installer tests (AFSC_ACFS_REPO or the default path)
acfs_repo_path() {
    local candidate="${AFSC_ACFS_REPO:-/data/projects/agentic_coding_flywheel_setup}"
    if [[ -f "$candidate/checksums.yaml" ]]; then
        echo "$candidate"
    fi
}

# docker_fixture_config — config that runs mock installers inside containers: the fixtures dir
# is bind-mounted read-only at /fixtures and entries use file:///fixtures/<name>.sh.
docker_fixture_config() {
    write_config "[docker]
volumes = [\"$TEST_TMP/fixtures:/fixtures:ro\"]
timeout_seconds = ${E2E_DOCKER_TIMEOUT:-120}
$1"
}

# add_docker_installer name — like add_installer, but the URL points inside the container mount
add_docker_installer() {
    local name="$1"
    local path="$TEST_TMP/fixtures/$name.sh"
    cat > "$path"
    chmod +x "$path"
    add_entry "$name" "file:///fixtures/$name.sh" "$(sha256_file "$path")"
    echo "$path"
}
