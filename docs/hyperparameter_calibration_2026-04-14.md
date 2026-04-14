# CLUSDT/xyz:CL Hyperparameter Calibration Report

> Calibrated from 3 live spread recordings on 2026-04-14 (Binance maker, Hyperliquid hedge)

## 1. Data Sources

| Case | BJT Window | Duration | Samples | Description |
|------|-----------|----------|---------|-------------|
| Case 1 | 05:30-07:30 | 2h | 7,200 | Early morning, low liquidity, ends with news shock (WTI crash) |
| Case 2 | 13:30-15:30 | 2h | 7,200 | Afternoon, stable market, flash crash at end (both sides sync) |
| Case 3 | 19:40-00:36 | 4.9h | 17,764 | Live trading session, gradual WTI decline then recovery |

## 2. Normal Regime Characterization

**Data used**: Case 2 full + Case 3 first 2/3 (before wide-spread drift), total 16,642 samples.

Excluded Case 1 from normal calibration because its 50 bps baseline is anomalous (Binance maker_mid was pinned at $93.37 throughout, indicating a low-liquidity or stale-price condition).

### Mid Spread Distribution (|Binance mid - HL mid|, bps)

| Statistic | Value |
|-----------|-------|
| Mean | 18.3 |
| Median | 17.9 |
| P5 | 8.7 |
| P25 | 13.5 |
| P75 | 20.8 |
| P95 | 29.7 |
| Std Dev | 7.8 |
| Direction | >99% positive (Binance > HL) |

### Sell Edge Distribution (bps, higher = more profitable)

| Statistic | Value |
|-----------|-------|
| Mean | 16.8 |
| P5 | 7.1 |
| P25 | 12.1 |

### HL Spread (bps, determines hedge slippage)

| Statistic | Value |
|-----------|-------|
| Mean | 1.44 |
| P95 | 3.13 |
| P99 | 3.94 |

**Key observations**:
- Spread is structurally positive and above total fees (5.5 bps) >97% of the time
- The spread mean of ~18 bps leaves ~12.5 bps net after fees
- Spread is very stable (std = 7.8 bps), no sudden regime shifts in normal conditions
- HL taker spread is tight (1.4 bps mean), hedge slippage is minimal

## 3. Event Regime Characterization

**Data used**: Case 1 tail 30 min (event shock, 130-230 bps) + Case 3 wide-spread period 14:30-16:00 UTC (post-crash drift, 25-44 bps), total 7,200 samples.

### Event Spread Distribution (bps)

| Statistic | Value |
|-----------|-------|
| Mean | 58.0 |
| Median | 31.7 |
| P25 | 29.1 |
| P75 | 70.2 |
| P95 | 165.8 |
| Std Dev | 50.2 |

**Key observations**:
- Event spreads are highly heterogeneous: Case 1 shock reached 230 bps, Case 3 drift only 35-40 bps
- Even at P25 of event regime (29 bps), there's still 23 bps net profit after fees
- Event mode should tolerate wider cancel thresholds to avoid premature exits during volatile but profitable periods

## 4. Parameter Derivation Logic

### Fees (fixed)
- Binance maker fee: 1.5 bps
- Hyperliquid taker fee: 4.0 bps
- **Total fee: 5.5 bps**

### Normal Mode

| Parameter | Formula | Value |
|-----------|---------|-------|
| `profit_rate_bps` | sell_edge P25 (12.1) - fees (5.5) - safety margin (1.0) | **5.6** |
| `profit_cancel_threshold_bps` | spread std (7.8) x 1.5 | **11.7** |
| `order_notional_usd` | ~1 lot at $90 | **90** |
| `order_refresh_interval_secs` | Moderate (not too aggressive for rate limits) | **90** |

**Rationale**:
- `profit_rate_bps = 5.6`: At the P25 sell edge (12.1 bps), we still clear fees + profit + 1 bps safety. This means we'd trade at least 75% of the time. Setting it to the previous 16.0 would only trade when spread > ~21.5 bps (roughly P75), missing half the opportunities.
- `profit_cancel_threshold_bps = 11.7`: With 7.8 bps std, a threshold of 1.5x std avoids cancelling on normal noise. The previous value of 4.0 was too tight (within 0.5x std), causing 57,642 cancel triggers in Case 3.

### Event Mode

| Parameter | Formula | Value |
|-----------|---------|-------|
| `profit_rate_bps` | event_spread P25 (29.1) - fees (5.5) - safety margin (2.0) | **21.6** |
| `profit_cancel_threshold_bps` | event_spread std (50.2) x 0.5 | **25.1** |
| `order_notional_usd` | 2x normal (wider spread = more edge to capture) | **180** |
| `order_refresh_interval_secs` | Shorter (capture fast-moving opportunities) | **60** |

**Rationale**:
- `profit_rate_bps = 21.6`: At P25 of event regime, still clears fees + profit. Previous 42.0 was too conservative (would only trade at P60+ of event spreads).
- `profit_cancel_threshold_bps = 25.1`: Event spreads are volatile (50 bps std). A 25 bps threshold prevents premature cancellation during recoveries. Previous 8.0 would thrash constantly.
- `order_notional_usd = 180`: Double normal. When edge is 3x wider, deploying 2x capital is reasonable.

### Event Trigger

| Parameter | Formula | Value |
|-----------|---------|-------|
| `event_trigger_mid_spread_bps` | normal P95 (29.7) x 2.0 | **59** |
| `event_trigger_hl_spread_bps` | normal HL_spread P99 (3.94) x 1.5 | **5.9** |
| `event_trigger_confirm_secs` | Short confirmation to avoid missing fast events | **3** |
| `event_rearm_cooldown_secs` | Prevent rapid on/off oscillation | **120** |

**Rationale**:
- `event_trigger_mid_spread_bps = 59`: The previous 100 was too high -- it would only trigger on Case 1-style extreme shocks, missing the moderate Case 3 widening (peaked at 44 bps in 5-min averages). 59 bps is ~2x normal P95, a clear departure from normal.
- `event_trigger_hl_spread_bps = 5.9`: Normal HL spread is almost always <4 bps. When it exceeds 6 bps, HL liquidity is deteriorating, confirming unusual conditions.

### Exit Rebalance

| Parameter | Formula | Value |
|-----------|---------|-------|
| `exit_rebalance_entry_spread_bps` | trigger x 0.8 | **47** |
| `exit_rebalance_exit_spread_bps` | normal P75 | **21** |
| `exit_rebalance_confirm_secs` | Unchanged | **10** |
| `exit_rebalance_cooldown_secs` | Unchanged | **120** |
| `exit_rebalance_min_abs_position` | Unchanged | **0.01** |

**Rationale**:
- Entry at 47 bps: begin monitoring for exit when spread enters the event zone but hasn't peaked yet.
- Exit at 21 bps: normal P75. When spread returns to the upper range of normal, the event is considered over and positions should be flattened.

## 5. Recommended Config

```json
{
  "maker_exchange": "binance",
  "symbol": "CLUSDT",
  "maker_symbol": "CLUSDT",
  "hedge_symbol": "xyz:CL",
  "strategy_mode": "dual",
  "continuous_mode": true,

  "pacifica_maker_fee_bps": 1.5,
  "hyperliquid_taker_fee_bps": 4.0,

  "normal_mode": {
    "profit_rate_bps": 5.6,
    "order_notional_usd": 90.0,
    "profit_cancel_threshold_bps": 11.7,
    "order_refresh_interval_secs": 90
  },
  "event_mode": {
    "profit_rate_bps": 21.6,
    "order_notional_usd": 180.0,
    "profit_cancel_threshold_bps": 25.1,
    "order_refresh_interval_secs": 60
  },

  "event_trigger_mid_spread_bps": 59.0,
  "event_trigger_hl_spread_bps": 5.9,
  "event_trigger_confirm_secs": 3,
  "event_rearm_cooldown_secs": 120,

  "enable_exit_rebalance": true,
  "exit_rebalance_entry_spread_bps": 47.0,
  "exit_rebalance_exit_spread_bps": 21.0,
  "exit_rebalance_confirm_secs": 10,
  "exit_rebalance_cooldown_secs": 120,
  "exit_rebalance_min_abs_position": 0.01,

  "evaluation_loop_interval_ms": 50,
  "order_failure_cooldown_ms": 1500,
  "post_only_reject_cooldown_ms": 6000,
  "hyperliquid_slippage": 0.05,
  "reconnect_attempts": 5,
  "ping_interval_secs": 15,
  "pacifica_rest_poll_interval_secs": 2,
  "pacifica_active_order_rest_poll_interval_ms": 500
}
```

## 6. Previous vs New Parameters

| Parameter | Previous | New | Change | Reason |
|-----------|----------|-----|--------|--------|
| normal profit_rate_bps | 16.0 | **5.6** | -65% | Previous too conservative; missed P25-P75 of opportunities |
| normal cancel_threshold_bps | 4.0 | **11.7** | +193% | Previous was within 0.5x std, causing 57K cancel storms |
| event profit_rate_bps | 42.0 | **21.6** | -49% | Previous missed most of the event recovery window |
| event cancel_threshold_bps | 8.0 | **25.1** | +214% | Event spread is 50 bps std; 8 bps was way too tight |
| event_trigger_mid_spread_bps | 100.0 | **59.0** | -41% | Previous only triggered on extreme shocks; missed moderate events |
| event_trigger_hl_spread_bps | 8.0 | **5.9** | -26% | Better aligned with actual HL spread P99 |
| event order_notional_usd | 90.0 | **180.0** | +100% | Capture more from wider event spreads |
| exit_rebalance_exit_spread_bps | 40.0 | **21.0** | -48% | Previous let event linger too long; 21=normal P75 is more precise |

## 7. Expected Impact

### Normal regime ($90 notional, ~$90 price)
- Previous: $90 x 16.0/10000 = $0.144 target per trade, trades at ~P75 of spread
- **New: $90 x 5.6/10000 = $0.050 target per trade, trades at ~P25 of spread**
- Lower per-trade profit but ~3x more trades -> higher aggregate throughput

### Event regime ($180 notional)
- Previous: $90 x 42.0/10000 = $0.378 target, triggers only at 100+ bps
- **New: $180 x 21.6/10000 = $0.389 target, triggers at 59+ bps**
- Similar per-trade profit but captures moderate events that previous config missed entirely

### Cancel pressure reduction
- `profit_cancel_threshold_bps`: 4.0 -> 11.7 (normal), 8.0 -> 25.1 (event)
- Expected >90% reduction in cancel API calls, dramatically reducing rate-limit risk
