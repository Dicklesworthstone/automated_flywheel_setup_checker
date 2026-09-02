#!/usr/bin/env bash
# Install (or preview) the Flywheel Checker systemd units.
#
# Renders systemd/*.service.in with the placeholders below, installs the binary, creates the
# data/log/config directories, writes a config with the right paths, installs logrotate, and
# enables the nightly timer plus the serve unit. `--dry-run` prints the rendered units and every
# action without touching the system.
#
#   @BIN@         installed binary                  (default /usr/local/bin/automated_flywheel_setup_checker)
#   @USER@        service user (must be in `docker`) (default: $SUDO_USER or current user)
#   @DATA_DIR@    results, metrics, locks, logs     (default /var/lib/flywheel-checker)
#   @LOG_DIR@     structured event log               (default /var/log/flywheel-checker)
#   @CONFIG_DIR@  /etc/flywheel-checker
#   @CONFIG@      @CONFIG_DIR@/config.toml
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

DRY_RUN=0
BIN_SRC=""
USER_NAME="${SUDO_USER:-$(id -un)}"
DATA_DIR="/var/lib/flywheel-checker"
LOG_DIR="/var/log/flywheel-checker"
CONFIG_DIR="/etc/flywheel-checker"
BIN_DST="/usr/local/bin/automated_flywheel_setup_checker"
ACFS_REPO=""
OUT_DIR=""
ENABLE_SERVE=1

usage() {
    cat <<EOF
Usage: $0 [--dry-run] [--bin PATH] [--user NAME] [--data-dir DIR] [--log-dir DIR]
          [--acfs-repo DIR] [--out-dir DIR] [--no-serve]

  --dry-run        Render units to stdout (or --out-dir) and print actions; change nothing
  --bin PATH       Binary to install (default: target/release/automated_flywheel_setup_checker)
  --user NAME      Service user (default: \$SUDO_USER or current user); must be in the docker group
  --data-dir DIR   Results/metrics/locks (default: $DATA_DIR)
  --log-dir DIR    Structured event log (default: $LOG_DIR)
  --acfs-repo DIR  ACFS checkout written into the config as [general].acfs_repo
  --out-dir DIR    Write rendered units here instead of stdout (dry-run) or /etc/systemd/system
  --no-serve       Do not enable the health/metrics serve unit
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --bin) BIN_SRC="$2"; shift ;;
        --user) USER_NAME="$2"; shift ;;
        --data-dir) DATA_DIR="$2"; shift ;;
        --log-dir) LOG_DIR="$2"; shift ;;
        --acfs-repo) ACFS_REPO="$2"; shift ;;
        --out-dir) OUT_DIR="$2"; shift ;;
        --no-serve) ENABLE_SERVE=0 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

CONFIG="$CONFIG_DIR/config.toml"
BIN_SRC="${BIN_SRC:-$PROJECT_ROOT/target/release/automated_flywheel_setup_checker}"

render() {
    # $1 = template path; stdout = rendered unit
    sed -e "s|@BIN@|$BIN_DST|g" \
        -e "s|@USER@|$USER_NAME|g" \
        -e "s|@DATA_DIR@|$DATA_DIR|g" \
        -e "s|@LOG_DIR@|$LOG_DIR|g" \
        -e "s|@CONFIG_DIR@|$CONFIG_DIR|g" \
        -e "s|@CONFIG@|$CONFIG|g" \
        "$1"
}

run() {
    if [[ $DRY_RUN -eq 1 ]]; then
        echo "[dry-run] $*"
    else
        "$@"
    fi
}

UNITS=(automated-flywheel-checker.service automated-flywheel-checker-emergency.service automated-flywheel-checker-serve.service)

echo "Flywheel Checker systemd install (user=$USER_NAME data=$DATA_DIR logs=$LOG_DIR config=$CONFIG)"
[[ $DRY_RUN -eq 1 ]] && echo "DRY RUN: nothing will be changed"

if [[ -n "$OUT_DIR" ]]; then
    mkdir -p "$OUT_DIR"
fi

# 1. Render units
for unit in "${UNITS[@]}"; do
    template="$PROJECT_ROOT/systemd/$unit.in"
    [[ -f "$template" ]] || { echo "missing template $template" >&2; exit 1; }
    if [[ -n "$OUT_DIR" ]]; then
        render "$template" > "$OUT_DIR/$unit"
        echo "rendered $OUT_DIR/$unit"
    elif [[ $DRY_RUN -eq 1 ]]; then
        echo "----- $unit -----"
        render "$template"
    fi
done
if [[ -n "$OUT_DIR" ]]; then
    cp "$PROJECT_ROOT/systemd/automated-flywheel-checker.timer" "$OUT_DIR/"
    echo "rendered $OUT_DIR/automated-flywheel-checker.timer"
fi
if [[ $DRY_RUN -eq 1 && -z "$OUT_DIR" ]]; then
    echo "----- automated-flywheel-checker.timer -----"
    cat "$PROJECT_ROOT/systemd/automated-flywheel-checker.timer"
fi

# 2. Binary
if [[ ! -x "$BIN_SRC" && $DRY_RUN -eq 0 ]]; then
    echo "binary not found at $BIN_SRC (build with: cargo build --release, or pass --bin)" >&2
    exit 1
fi
run sudo install -m 0755 "$BIN_SRC" "$BIN_DST"

# 3. Directories owned by the service user
run sudo mkdir -p "$DATA_DIR" "$LOG_DIR" "$CONFIG_DIR"
run sudo chown "$USER_NAME:$USER_NAME" "$DATA_DIR" "$LOG_DIR"

# 4. Config (never overwrite an existing one)
if [[ -f "$CONFIG" && $DRY_RUN -eq 0 ]]; then
    echo "config exists at $CONFIG; leaving it alone"
else
    tmp="$(mktemp)"
    {
        cat "$PROJECT_ROOT/config/default.toml"
    } > "$tmp"
    # Point the service at its directories (sed on the documented keys of default.toml).
    sed -i -e "s|^data_dir = .*|data_dir = \"$DATA_DIR\"|" \
           -e "s|^log_dir = .*|log_dir = \"$LOG_DIR\"|" "$tmp"
    if [[ -n "$ACFS_REPO" ]]; then
        sed -i -e "s|^acfs_repo = .*|acfs_repo = \"$ACFS_REPO\"|" "$tmp"
    fi
    if [[ $DRY_RUN -eq 1 ]]; then
        echo "[dry-run] would write $CONFIG with data_dir=$DATA_DIR log_dir=$LOG_DIR${ACFS_REPO:+ acfs_repo=$ACFS_REPO}"
        rm -f "$tmp"
    else
        sudo install -m 0644 "$tmp" "$CONFIG"
        rm -f "$tmp"
        echo "wrote $CONFIG"
    fi
fi

# 5. logrotate (paths match @LOG_DIR@ only when it is the default; otherwise rewrite them)
tmp_lr="$(mktemp)"
sed -e "s|/var/log/flywheel-checker|$LOG_DIR|g" "$PROJECT_ROOT/systemd/logrotate-flywheel-checker" \
    -e "s|create 0644 ubuntu ubuntu|create 0644 $USER_NAME $USER_NAME|" > "$tmp_lr"
run sudo install -m 0644 "$tmp_lr" /etc/logrotate.d/flywheel-checker
rm -f "$tmp_lr"

# 6. Units
if [[ $DRY_RUN -eq 0 ]]; then
    for unit in "${UNITS[@]}"; do
        tmp_u="$(mktemp)"
        render "$PROJECT_ROOT/systemd/$unit.in" > "$tmp_u"
        sudo install -m 0644 "$tmp_u" "/etc/systemd/system/$unit"
        rm -f "$tmp_u"
    done
    sudo install -m 0644 "$PROJECT_ROOT/systemd/automated-flywheel-checker.timer" /etc/systemd/system/
    if command -v systemd-analyze >/dev/null 2>&1; then
        systemd-analyze verify /etc/systemd/system/automated-flywheel-checker.service \
            /etc/systemd/system/automated-flywheel-checker-serve.service || true
    fi
    sudo systemctl daemon-reload
    sudo systemctl enable --now automated-flywheel-checker.timer
    if [[ $ENABLE_SERVE -eq 1 ]]; then
        sudo systemctl enable --now automated-flywheel-checker-serve.service || true
    fi
else
    echo "[dry-run] install units: ${UNITS[*]} automated-flywheel-checker.timer -> /etc/systemd/system"
    echo "[dry-run] systemctl daemon-reload; enable --now automated-flywheel-checker.timer${ENABLE_SERVE:+; enable --now automated-flywheel-checker-serve.service}"
fi

cat <<EOF

Done. Useful commands:
  Manual run:      sudo systemctl start automated-flywheel-checker.service
  On-demand run:   sudo systemctl start automated-flywheel-checker-emergency.service
  Logs:            journalctl -u automated-flywheel-checker.service -f
  Event log:       $LOG_DIR/checker_YYYYMMDD.jsonl
  Results:         $BIN_DST --config $CONFIG status --list
  Health/metrics:  enable [monitoring] in $CONFIG (serve unit: automated-flywheel-checker-serve.service)
  Doctor:          $BIN_DST --config $CONFIG doctor
EOF
