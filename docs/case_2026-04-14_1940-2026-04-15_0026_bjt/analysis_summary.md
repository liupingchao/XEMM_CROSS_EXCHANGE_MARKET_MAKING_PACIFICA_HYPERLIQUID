# Case Analysis Summary

## Window
- Beijing time: 2026-04-14 19:40 to 2026-04-15 00:26
- UTC: 2026-04-14 11:40:39 to 2026-04-14 16:26:xx
- Source log: `xemm_runtime.log`

## Data snapshot
- Binance orders: 918
- Binance fills: 28
- Hyperliquid hedge orders: 28
- Hyperliquid hedge fills: 28

## Fill/hedge consistency
- Binance net qty (SELL negative): -27.13
- Hyperliquid net qty (BUY positive): +27.13
- Net delta across two venues: ~0.00 (qty matched)
- Binance fill sources:
  - `binance_user_stream`: 22
  - `rest_trade_history_recovery`: 6

## Execution quality (this window)
- Binance fill notional: 2502.96 USDT
- Hyperliquid fill notional: 2498.99 USDT
- Gross cross-venue notional difference: +3.97 USDT
- Approx VWAP:
  - Binance SELL VWAP: 92.2579
  - Hyperliquid BUY VWAP: 92.1117

## Runtime behavior
- Strategy mode in log: `Dual`
- Event-mode activations: 0
- Exit/rebalance triggers: 0
- Orders were entirely from `main_loop_limit_order_normal`.

## Operational risk signals
- `[CANCEL] Profit deviation`: 57,642 (very frequent)
- `[CANCEL] Age expiry`: 6
- `Failed to place order`: 738
- `Rate limit hit` (REST fill detection): 1,028
- Binance 418/403 occurrences in log:
  - 418 (`-1003` banned): 79
  - 403 Forbidden: 22,713

## Phase observation
1. 19:43-21:37 BJT: normal mode had continuous fills and successful 1:1 hedge closure.
2. 21:37-00:26 BJT: still many order attempts, but no new fills recorded in snapshot; logs show frequent Binance access errors/rate-limit pressure.

## Conclusion
- Core trading loop and hedge linkage are functional in this case window (fills and hedge quantities match).
- Current bottleneck is exchange/API access stability and cancel/retry pressure, not hedge execution correctness.
- Event branch was not exercised in this window.
