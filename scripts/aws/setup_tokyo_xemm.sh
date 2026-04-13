#!/usr/bin/env bash
set -euo pipefail

# Tokyo AWS bootstrap for XEMM bot (Ubuntu 22.04/24.04)
#
# Usage:
#   chmod +x scripts/aws/setup_tokyo_xemm.sh
#   ./scripts/aws/setup_tokyo_xemm.sh
#
# Optional env vars:
#   REPO_URL=https://github.com/<you>/<repo>.git
#   APP_DIR=/home/ubuntu/XEMM_rust
#   BRANCH=main
#   SERVICE_NAME=xemm
#   APP_USER=ubuntu

REPO_URL="${REPO_URL:-}"
APP_DIR="${APP_DIR:-/home/ubuntu/XEMM_rust}"
BRANCH="${BRANCH:-main}"
SERVICE_NAME="${SERVICE_NAME:-xemm}"
APP_USER="${APP_USER:-ubuntu}"

if [[ "$(id -u)" -eq 0 ]]; then
  echo "Please run as non-root user with sudo privilege (e.g., ubuntu)."
  exit 1
fi

if ! command -v sudo >/dev/null 2>&1; then
  echo "sudo is required"
  exit 1
fi

echo "[1/8] Set timezone to Asia/Tokyo"
sudo timedatectl set-timezone Asia/Tokyo || true

echo "[2/8] Install system packages"
sudo apt-get update -y
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
  build-essential \
  pkg-config \
  libssl-dev \
  git \
  curl \
  ca-certificates \
  jq \
  tmux \
  ufw

echo "[3/8] Install Rust toolchain (if missing)"
if [[ ! -x "$HOME/.cargo/bin/rustup" ]]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
source "$HOME/.cargo/env"
rustup default stable >/dev/null

echo "[4/8] Prepare project directory"
if [[ -n "$REPO_URL" ]]; then
  if [[ -d "$APP_DIR/.git" ]]; then
    git -C "$APP_DIR" fetch --all --prune
    git -C "$APP_DIR" checkout "$BRANCH"
    git -C "$APP_DIR" pull --ff-only
  else
    git clone --branch "$BRANCH" "$REPO_URL" "$APP_DIR"
  fi
else
  if [[ ! -d "$APP_DIR" ]]; then
    echo "APP_DIR does not exist and REPO_URL is empty."
    echo "Set REPO_URL or copy project to: $APP_DIR"
    exit 1
  fi
fi

echo "[5/8] Build release binary"
cd "$APP_DIR"
source "$HOME/.cargo/env"
cargo build --release

echo "[6/8] Prepare local config files"
if [[ ! -f "$APP_DIR/.env" ]]; then
  cp "$APP_DIR/.env.example" "$APP_DIR/.env"
  chmod 600 "$APP_DIR/.env"
  echo "Created $APP_DIR/.env from .env.example"
fi

if [[ ! -f "$APP_DIR/config.json" ]]; then
  echo "Missing config.json at $APP_DIR/config.json"
  exit 1
fi

echo "[7/8] Install systemd service: $SERVICE_NAME"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
sudo tee "$SERVICE_FILE" >/dev/null <<EOF
[Unit]
Description=XEMM Rust Bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${APP_USER}
Group=${APP_USER}
WorkingDirectory=${APP_DIR}
EnvironmentFile=${APP_DIR}/.env
Environment=RUST_LOG=info
Environment=RUST_BACKTRACE=1
ExecStart=/bin/bash -lc 'source /home/${APP_USER}/.cargo/env && ${APP_DIR}/target/release/xemm_rust'
Restart=always
RestartSec=2
LimitNOFILE=65535
KillSignal=SIGINT
TimeoutStopSec=25

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable "${SERVICE_NAME}"

echo "[8/8] Security hardening (optional UFW)"
echo "UFW currently allows SSH only."
sudo ufw allow OpenSSH >/dev/null 2>&1 || true
sudo ufw --force enable >/dev/null 2>&1 || true

echo
echo "Setup complete."
echo "Next steps:"
echo "1) Edit ${APP_DIR}/.env and ${APP_DIR}/config.json"
echo "2) Start service:   sudo systemctl start ${SERVICE_NAME}"
echo "3) Check status:    sudo systemctl status ${SERVICE_NAME}"
echo "4) Tail logs:       sudo journalctl -u ${SERVICE_NAME} -f --no-pager"
