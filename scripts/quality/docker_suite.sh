#!/usr/bin/env bash
# Docker suite for the dsr gate: build the `docker` test executable on an rch worker (cargo never
# runs on a shared host), bring only that executable back, and run it against THIS host's Docker
# daemon, where the prepared images are cached and the daemon is known-good. Workers' Docker
# daemons proved unreliable for the cold image build (nvm/node downloads failing, >900 s builds).
#
# Usage: scripts/quality/docker_suite.sh            (needs docker on this host, rch configured)
#        AFSC_DOCKER_SUITE_SKIP_BUILD=1 …           (reuse .afsc-remote-out/docker-suite)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
OUT=.afsc-remote-out
BIN="$OUT/docker-suite"

if ! docker info > /dev/null 2>&1; then
    echo "docker_suite: no Docker daemon on this host; the suite cannot run here" >&2
    exit 3
fi

if [[ "${AFSC_DOCKER_SUITE_SKIP_BUILD:-0}" != "1" ]]; then
    mkdir -p "$OUT"
    # --job runs an arbitrary command remotely and syncs --result-dir back; cargo's JSON output
    # names the test executable so we copy exactly that file.
    RCH_REQUIRE_REMOTE=1 RCH_BUILD_TIMEOUT_SEC=3600 rch exec --job --result-dir "$OUT" -- bash -c \
        'set -e; mkdir -p '"$OUT"'; exe=$(cargo test --locked --test docker --no-run --message-format=json 2>/dev/null | jq -r "select(.reason==\"compiler-artifact\" and .target.name==\"docker\" and .executable!=null) | .executable" | tail -1); test -n "$exe"; cp "$exe" '"$BIN"'; chmod +x '"$BIN"
fi

test -x "$BIN" || { echo "docker_suite: $BIN missing after the remote build" >&2; exit 6; }
echo "docker_suite: running $BIN against the local daemon"
AFSC_DOCKER_TESTS=1 "$BIN" --test-threads=1
