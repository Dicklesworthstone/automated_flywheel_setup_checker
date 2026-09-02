#!/bin/bash
# ============================================================
# E2E Test Runner for Automated Flywheel Setup Checker
#
# Runs comprehensive end-to-end tests that validate the entire
# installer testing pipeline in realistic conditions.
#
# Related: bead bd-19y9.1.8
#
# Usage:
#   ./scripts/e2e/run_all_tests.sh [options]
#
# Options:
#   E2E_FILTER="pattern"   - Filter tests by name regex
#   E2E_VERBOSE=1          - Enable verbose output
#   E2E_TIMEOUT=1800       - Per-test timeout in seconds
#   E2E_PARALLEL=1         - Number of parallel tests (future)
#   E2E_SKIP_PREFLIGHT=1   - Skip preflight checks
#
# Requirements:
#   - Docker daemon running
#   - cargo build --release completed
#   - jq installed
# ============================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="${PROJECT_ROOT}/target/e2e-logs"
REPORT_DIR="${PROJECT_ROOT}/target/e2e-reports"

mkdir -p "$LOG_DIR" "$REPORT_DIR"

# Source library files
source "$SCRIPT_DIR/lib/assertions.sh"
source "$SCRIPT_DIR/lib/helpers.sh"

# Configuration
E2E_TIMEOUT="${E2E_TIMEOUT:-1800}"  # 30 minutes max per test
E2E_FILTER="${E2E_FILTER:-}"
E2E_VERBOSE="${E2E_VERBOSE:-0}"
E2E_PARALLEL="${E2E_PARALLEL:-1}"
E2E_SKIP_PREFLIGHT="${E2E_SKIP_PREFLIGHT:-0}"

# Test results tracking
declare -a PASSED_TESTS=()
declare -a FAILED_TESTS=()
declare -a SKIPPED_TESTS=()
START_TIME=$(date +%s)

# Logging functions
log_info() { echo -e "\033[34m[INFO]\033[0m $*"; }
log_pass() { echo -e "\033[32m[PASS]\033[0m $*"; }
log_fail() { echo -e "\033[31m[FAIL]\033[0m $*"; }
log_warn() { echo -e "\033[33m[WARN]\033[0m $*"; }
log_debug() { [[ "$E2E_VERBOSE" == "1" ]] && echo -e "\033[90m[DEBUG]\033[0m $*" || true; }

# Pre-flight checks
preflight_check() {
    log_info "Running pre-flight checks..."

    # Docker is optional: Docker-backed scripts skip themselves when no daemon is reachable.
    if ! docker info > /dev/null 2>&1; then
        log_warn "Docker not available: Docker-backed scenarios will be skipped"
    else
        log_debug "Docker: OK"
    fi

    # Check jq
    if ! command -v jq &>/dev/null; then
        log_fail "jq not installed"
        exit 1
    fi
    log_debug "jq: OK"

    # Check binary exists
    local binary="$PROJECT_ROOT/target/release/automated_flywheel_setup_checker"
    if [[ ! -f "$binary" ]]; then
        log_warn "Binary not found, building..."
        if ! cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>&1; then
            log_fail "Failed to build binary"
            exit 1
        fi
    fi
    log_debug "Binary: OK"

    # Check test fixtures directory exists
    local fixtures_dir="$PROJECT_ROOT/tests/fixtures"
    if [[ ! -d "$fixtures_dir" ]]; then
        log_warn "Test fixtures directory not found, creating..."
        mkdir -p "$fixtures_dir/error_outputs" "$fixtures_dir/installers"
    fi
    log_debug "Fixtures: OK"

    log_pass "Pre-flight checks passed"
}

# Run single test with timeout and logging
run_test() {
    local test_name="$1"
    local test_script="$SCRIPT_DIR/tests/${test_name}.sh"
    local test_log="$LOG_DIR/${test_name}.log"
    local test_start
    test_start=$(date +%s)

    if [[ ! -f "$test_script" ]]; then
        log_warn "Test script not found: $test_script"
        SKIPPED_TESTS+=("$test_name")
        return 0
    fi

    # Check if test should be filtered
    if [[ -n "$E2E_FILTER" ]] && [[ ! "$test_name" =~ $E2E_FILTER ]]; then
        log_debug "Skipping $test_name (filtered)"
        SKIPPED_TESTS+=("$test_name")
        return 0
    fi

    log_info "Running: $test_name"

    # Ensure test script is executable
    chmod +x "$test_script"

    # Run with timeout and capture output
    local exit_code=0
    if timeout "$E2E_TIMEOUT" bash "$test_script" > "$test_log" 2>&1; then
        local test_end
        test_end=$(date +%s)
        local duration=$((test_end - test_start))
        log_pass "$test_name (${duration}s)"
        PASSED_TESTS+=("$test_name")
    else
        exit_code=$?
        local test_end
        test_end=$(date +%s)
        local duration=$((test_end - test_start))
        log_fail "$test_name (${duration}s, exit=$exit_code)"
        FAILED_TESTS+=("$test_name")

        # Capture diagnostics on failure
        capture_diagnostics "$test_name" "$test_log"

        # Show last few lines of log in verbose mode
        if [[ "$E2E_VERBOSE" == "1" ]]; then
            log_debug "Last 10 lines of $test_log:"
            tail -10 "$test_log" | while read -r line; do
                log_debug "  $line"
            done
        fi
    fi
}

# Diagnostic capture on test failure
capture_diagnostics() {
    local test_name="$1"
    local test_log="$2"
    local diag_dir="$LOG_DIR/diagnostics/${test_name}"

    mkdir -p "$diag_dir"

    # Copy test log
    cp "$test_log" "$diag_dir/"

    # Docker state
    docker ps -a > "$diag_dir/docker_ps.txt" 2>&1 || true

    # Get logs from recent test containers
    local containers
    containers=$(docker ps -aq --filter "name=acfs-test-" | head -5)
    if [[ -n "$containers" ]]; then
        for container in $containers; do
            docker logs "$container" > "$diag_dir/docker_logs_${container}.txt" 2>&1 || true
        done
    fi

    # System state
    free -h > "$diag_dir/memory.txt" 2>&1 || true
    df -h > "$diag_dir/disk.txt" 2>&1 || true

    # Application logs if they exist
    if [[ -d "$PROJECT_ROOT/target/logs" ]]; then
        cp -r "$PROJECT_ROOT/target/logs" "$diag_dir/app_logs" 2>/dev/null || true
    fi

    log_debug "Diagnostics captured: $diag_dir"
}

# Generate JSON and Markdown reports
generate_reports() {
    local end_time
    end_time=$(date +%s)
    local total_duration=$((end_time - START_TIME))
    local total_tests=$((${#PASSED_TESTS[@]} + ${#FAILED_TESTS[@]} + ${#SKIPPED_TESTS[@]}))

    # JSON report
    {
        echo "{"
        echo "  \"timestamp\": \"$(date -Iseconds)\","
        echo "  \"duration_seconds\": $total_duration,"
        echo "  \"summary\": {"
        echo "    \"total\": $total_tests,"
        echo "    \"passed\": ${#PASSED_TESTS[@]},"
        echo "    \"failed\": ${#FAILED_TESTS[@]},"
        echo "    \"skipped\": ${#SKIPPED_TESTS[@]}"
        echo "  },"

        # Passed tests array
        echo -n "  \"passed\": ["
        local first=true
        for t in "${PASSED_TESTS[@]}"; do
            $first && first=false || echo -n ","
            echo -n "\"$t\""
        done
        echo "],"

        # Failed tests array
        echo -n "  \"failed\": ["
        first=true
        for t in "${FAILED_TESTS[@]}"; do
            $first && first=false || echo -n ","
            echo -n "\"$t\""
        done
        echo "],"

        # Skipped tests array
        echo -n "  \"skipped\": ["
        first=true
        for t in "${SKIPPED_TESTS[@]}"; do
            $first && first=false || echo -n ","
            echo -n "\"$t\""
        done
        echo "]"

        echo "}"
    } > "$REPORT_DIR/results.json"

    # Markdown report
    {
        echo "# E2E Test Results"
        echo ""
        echo "**Date:** $(date)"
        echo "**Duration:** ${total_duration}s"
        echo ""
        echo "## Summary"
        echo ""
        echo "| Status | Count |"
        echo "|--------|-------|"
        echo "| Passed | ${#PASSED_TESTS[@]} |"
        echo "| Failed | ${#FAILED_TESTS[@]} |"
        echo "| Skipped | ${#SKIPPED_TESTS[@]} |"
        echo "| **Total** | **$total_tests** |"
        echo ""

        if [[ ${#PASSED_TESTS[@]} -gt 0 ]]; then
            echo "## Passed Tests"
            for t in "${PASSED_TESTS[@]}"; do
                echo "- ✓ $t"
            done
            echo ""
        fi

        if [[ ${#FAILED_TESTS[@]} -gt 0 ]]; then
            echo "## Failed Tests"
            for t in "${FAILED_TESTS[@]}"; do
                echo "- ✗ $t (see logs/diagnostics/$t/)"
            done
            echo ""
        fi

        if [[ ${#SKIPPED_TESTS[@]} -gt 0 ]]; then
            echo "## Skipped Tests"
            for t in "${SKIPPED_TESTS[@]}"; do
                echo "- ○ $t"
            done
            echo ""
        fi
    } > "$REPORT_DIR/results.md"

    log_info "Reports generated: $REPORT_DIR/"
}

# Cleanup orphan containers
cleanup_orphan_containers() {
    local prefix="${1:-acfs-test-}"
    local orphans
    orphans=$(docker ps -aq --filter "name=$prefix" 2>/dev/null || echo "")
    if [[ -n "$orphans" ]]; then
        log_warn "Cleaning up orphan containers: $orphans"
        docker rm -f $orphans > /dev/null 2>&1 || true
    fi
}

# Main execution
main() {
    log_info "Starting E2E test suite for Automated Flywheel Setup Checker"
    log_info "Project root: $PROJECT_ROOT"
    log_info "Log directory: $LOG_DIR"
    log_info "Filter: ${E2E_FILTER:-none}"
    log_info "Timeout: ${E2E_TIMEOUT}s per test"
    log_info "Verbose: $E2E_VERBOSE"
    echo ""

    # Pre-flight checks
    if [[ "$E2E_SKIP_PREFLIGHT" != "1" ]]; then
        preflight_check
    fi

    # Cleanup any leftover containers from previous runs
    cleanup_orphan_containers

    # Core test cases (13 tests)
    local core_tests=(
        "single_installer"
        "batch_run"
        "checksum_mismatch"
        "network_failure"
        "remediation_flow"
        "error_classification"
        "jsonl_output"
        "parallel_execution"
        "systemd_integration"
        "github_notification"
        "metrics_persistence"
        "config_override"
        "recovery_rollback"
    )

    # Docker-backed scenarios (skip without a daemon) plus real ACFS installers
    local edge_tests=(
        "container_timeout_handling"
        "out_of_memory_scenario"
        "disk_space_exhaustion"
        "network_partition_scenario"
        "real_installer_run"
    )

    # Run core tests
    log_info "Running core tests (13 tests)..."
    for test in "${core_tests[@]}"; do
        run_test "$test"
    done

    # Run edge case tests
    log_info "Running Docker-backed and real-installer tests (5 tests)..."
    for test in "${edge_tests[@]}"; do
        run_test "$test"
    done

    # Generate reports
    generate_reports

    # Final cleanup
    cleanup_orphan_containers

    # Summary
    echo ""
    echo "════════════════════════════════════════"
    echo "  E2E Test Summary"
    echo "════════════════════════════════════════"
    echo "  Passed:  ${#PASSED_TESTS[@]}"
    echo "  Failed:  ${#FAILED_TESTS[@]}"
    echo "  Skipped: ${#SKIPPED_TESTS[@]}"
    echo "════════════════════════════════════════"
    echo ""
    log_info "Logs: $LOG_DIR"
    log_info "Reports: $REPORT_DIR"

    # Exit with failure if any tests failed
    if [[ ${#FAILED_TESTS[@]} -gt 0 ]]; then
        log_fail "Some tests failed!"
        exit 1
    fi

    log_pass "All tests passed!"
    exit 0
}

main "$@"
