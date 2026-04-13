# AWS Tokyo Runbook (XEMM)

## 1. Create EC2
1. Region: `ap-northeast-1` (Tokyo)
2. AMI: Ubuntu 22.04 LTS or 24.04 LTS
3. Instance type: start with `c7i.large` / `c6i.large` (2 vCPU, 4 GB)
4. Security Group inbound:
   - TCP 22 from your office/home IP only
5. Storage: 30 GB gp3 is usually enough

## 2. SSH login
```bash
ssh -i <your_key>.pem ubuntu@<EC2_PUBLIC_IP>
```

## 3. Upload/clone repo
Option A (git clone):
```bash
git clone <YOUR_REPO_URL> /home/ubuntu/XEMM_rust
cd /home/ubuntu/XEMM_rust
```

Option B (from local):
```bash
scp -i <your_key>.pem -r . ubuntu@<EC2_PUBLIC_IP>:/home/ubuntu/XEMM_rust
```

## 4. Run setup script
```bash
cd /home/ubuntu/XEMM_rust
chmod +x scripts/aws/setup_tokyo_xemm.sh
./scripts/aws/setup_tokyo_xemm.sh
```

If repo is not present on server yet, use:
```bash
REPO_URL=<YOUR_REPO_URL> BRANCH=main ./scripts/aws/setup_tokyo_xemm.sh
```

## 5. Fill credentials and config
Edit env:
```bash
vim /home/ubuntu/XEMM_rust/.env
```

For Binance maker + Hyperliquid hedge, set at least:
```bash
BINANCE_API_KEY=...
BINANCE_API_SECRET=...
HL_WALLET=...
HL_PRIVATE_KEY=...
```

Edit config:
```bash
vim /home/ubuntu/XEMM_rust/config.json
```

Minimal config:
```json
{
  "maker_exchange": "binance",
  "symbol": "SOL",
  "reconnect_attempts": 5,
  "ping_interval_secs": 15,
  "pacifica_maker_fee_bps": 1.5,
  "hyperliquid_taker_fee_bps": 4.0,
  "profit_rate_bps": 15.0,
  "order_notional_usd": 20.0,
  "profit_cancel_threshold_bps": 3.0,
  "order_refresh_interval_secs": 60,
  "hyperliquid_slippage": 0.05,
  "pacifica_rest_poll_interval_secs": 2,
  "pacifica_active_order_rest_poll_interval_ms": 250
}
```

## 6. Start and monitor
Start:
```bash
sudo systemctl start xemm
```

Status:
```bash
sudo systemctl status xemm
```

Logs:
```bash
sudo journalctl -u xemm -f --no-pager
```

## 7. Update on server
```bash
cd /home/ubuntu/XEMM_rust
chmod +x scripts/aws/update_and_restart_xemm.sh
./scripts/aws/update_and_restart_xemm.sh
```

## 8. Safe operations checklist
1. Start with very small `order_notional_usd`
2. Confirm no stale positions before enabling service
3. Never use `kill -9`; use `systemctl stop xemm`
4. Keep API keys restricted and rotate periodically
