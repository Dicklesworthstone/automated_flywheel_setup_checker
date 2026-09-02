#!/usr/bin/env bash
# Uninstall the Flywheel Checker systemd units (config, data and logs are preserved).
set -euo pipefail

echo "Uninstalling Flywheel Checker systemd units"

echo "[1/3] Stopping and disabling..."
for unit in automated-flywheel-checker.timer automated-flywheel-checker.service \
            automated-flywheel-checker-emergency.service automated-flywheel-checker-serve.service; do
    sudo systemctl stop "$unit" 2>/dev/null || true
    sudo systemctl disable "$unit" 2>/dev/null || true
done

echo "[2/3] Removing units, logrotate config and the legacy notify wrapper..."
sudo rm -f /etc/systemd/system/automated-flywheel-checker.service \
           /etc/systemd/system/automated-flywheel-checker.timer \
           /etc/systemd/system/automated-flywheel-checker-emergency.service \
           /etc/systemd/system/automated-flywheel-checker-serve.service \
           /etc/logrotate.d/flywheel-checker \
           /usr/local/bin/notify-flywheel-failure

echo "[3/3] Reloading systemd..."
sudo systemctl daemon-reload

cat <<'EOF'

Uninstalled. Preserved: /etc/flywheel-checker (config), /var/lib/flywheel-checker (results),
/var/log/flywheel-checker (event log), /usr/local/bin/automated_flywheel_setup_checker (binary).
Remove them yourself if you want a clean slate.
EOF
