#!/usr/bin/env bash
# Legacy hook kept for old unit files: forwards to the built-in `notify --last-run`, which
# re-sends Slack/GitHub notifications for the most recent persisted run using the same
# configuration the checker itself uses ([notifications] in the config file).
#
# New installs do not use this script; the rendered units call `notify --last-run` directly.
set -euo pipefail

BIN="${AFSC_BIN:-/usr/local/bin/automated_flywheel_setup_checker}"
CONFIG="${ACFS_CONFIG:-${AFSC_CONFIG:-/etc/flywheel-checker/config.toml}}"

if [[ ! -x "$BIN" ]]; then
    echo "notify-flywheel-failure: $BIN not found; install with scripts/install-systemd.sh" >&2
    exit 1
fi

exec "$BIN" --config "$CONFIG" notify --last-run "$@"
