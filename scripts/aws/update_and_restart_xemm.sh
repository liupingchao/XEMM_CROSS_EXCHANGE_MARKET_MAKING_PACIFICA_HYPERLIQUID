#!/usr/bin/env bash
set -euo pipefail

# Pull latest code, rebuild release binary, restart systemd service.
#
# Usage:
#   chmod +x scripts/aws/update_and_restart_xemm.sh
#   ./scripts/aws/update_and_restart_xemm.sh
#
# Optional env vars:
#   APP_DIR=/home/ubuntu/XEMM_rust
#   BRANCH=main
#   SERVICE_NAME=xemm

APP_DIR="${APP_DIR:-/home/ubuntu/XEMM_rust}"
BRANCH="${BRANCH:-main}"
SERVICE_NAME="${SERVICE_NAME:-xemm}"

if [[ ! -d "$APP_DIR/.git" ]]; then
  echo "Not a git repo: $APP_DIR"
  exit 1
fi

source "$HOME/.cargo/env"

echo "[1/4] Pull latest code"
git -C "$APP_DIR" fetch --all --prune
git -C "$APP_DIR" checkout "$BRANCH"
git -C "$APP_DIR" pull --ff-only

echo "[2/4] Build release"
cd "$APP_DIR"
cargo build --release

echo "[3/4] Restart service"
sudo systemctl restart "$SERVICE_NAME"

echo "[4/4] Show status"
sudo systemctl --no-pager --full status "$SERVICE_NAME" | sed -n '1,25p'
echo
echo "Logs:"
echo "sudo journalctl -u $SERVICE_NAME -f --no-pager"
