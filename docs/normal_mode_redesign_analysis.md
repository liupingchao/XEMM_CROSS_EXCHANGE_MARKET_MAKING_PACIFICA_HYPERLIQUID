# Normal Mode Redesign: Mean-Reversion + Funding Carry

> Analysis date: 2026-04-15. To be implemented in next session.

## 1. Problem Statement

当前Normal mode每笔交易SELL Binance + BUY HL创建跨所spread position但**从不平仓**。Bot报告的"利润"是纸面价差，实际平仓需付同等价差+费用，导致净亏损。

Case 3实盘数据验证：
- Bot报告利润: +$2.59 (28笔交易)
- 真实平仓后: **-$3.56** (平仓价差$4.22 + 平仓费用$1.93 > 纸面利润)

## 2. Backtest Results: Mean-Reversion Viability

### 费用结构对比

| 场景 | 开仓费用 | 平仓费用 | Round-trip |
|------|---------|---------|-----------|
| 当前(平仓双边taker) | 5.5 bps | 8.0 bps | **13.5 bps** |
| **优化(平仓双边maker)** | 5.5 bps | **2.5 bps** | **8.0 bps** |

开仓: Binance maker(1.5) + HL taker(4.0) = 5.5 bps
平仓maker: Binance maker(1.5) + HL maker(~1.0) = 2.5 bps

### Taker close (13.5 bps RT): 所有参数组合均亏损

Case 2 (午后2h稳定数据):
- 93笔交易，胜率1%，总亏损 -865 bps
- 典型每笔: 赚3-5 bps spread收敛，但费用13.5 bps直接吃掉

### Maker close (8.0 bps RT): 高Z-score入场可盈利

**Case 2 参数扫描 (30分钟窗口):**
```
entry_z=2.0  exit_z=-0.5:  40 trades, win=35%, total= -10.8bps  (边缘)
entry_z=2.5  exit_z=-0.5:  24 trades, win=46%, total= +23.2bps  ✓
entry_z=3.0  exit_z=-0.5:  12 trades, win=75%, total= +28.3bps  ✓
entry_z=3.0  exit_z= 0.0:  12 trades, win=58%, total= +13.0bps  ✓
```

**Case 3 参数扫描:**
```
entry_z=2.0  exit_z=-0.5:  36 trades, win=36%, total= +14.1bps  ✓
entry_z=2.5  exit_z=-0.5:  25 trades, win=44%, total= +18.6bps  ✓
entry_z=3.0  exit_z=-0.5:  12 trades, win=75%, total= +56.0bps  ✓
entry_z=3.0  exit_z= 0.0:  12 trades, win=58%, total= +40.7bps  ✓
```

### 最佳参数区间

| 参数 | 推荐值 | 说明 |
|------|--------|------|
| `entry_z_score` | 2.5 ~ 3.0 | 只在大spike时开仓 (价差偏离均值2.5-3个标准差) |
| `exit_z_score` | -0.5 ~ 0.0 | 等价差充分回归到均值或均值以下才平仓 |
| `spread_window_secs` | 1800 | 30分钟滚动窗口 |
| Round-trip fee | 8.0 bps | 前提: 平仓走双边maker限价单 |

## 3. Approved Strategy Direction

用户确认的策略方向 (2026-04-15):

### Event/Spike Mode
- Event触发时**不管funding先开仓** (价差收敛是主要利润来源)
- 开仓后监控价差和funding
- **价差收敛 → maker限价单平仓**
- **Funding被交易所突然修改 → market order紧急平仓**

### Normal Mode
- **双边maker限价单平仓** (降低费用结构: 13.5→8.0 bps RT)
- 捕捉**中等波动** (15-20 bps swing, Z-score > 2.5)
- 同时捕捉**小价差的funding carry**
- 常态期间持仓收funding，只要net carry为正就有收益

### Funding决策逻辑
- 持仓时net carry > 0 → 放宽平仓条件 (让carry继续积累)
- 接近funding结算且carry < 0 → 加速平仓
- Funding rate被交易所突然修改 → market order紧急平仓

## 4. Implementation Plan (Next Session)

### 新文件
- `src/strategy/spread_stats.rs`: 滚动价差统计 (均值/std/Z-score)
- `src/strategy/funding.rs`: Funding carry计算

### 新API方法
- `src/connector/binance/trading.rs`: `get_premium_index()` → lastFundingRate, nextFundingTime
- `src/connector/hyperliquid/trading.rs`: `get_funding_rate()` → fundingRate, time

### 改造
- `src/connector/maker.rs`: trait增加 `get_funding_info()`, `place_limit_close_order()`
- `src/config.rs`: `ModeTradingConfig` 增加Z-score和funding参数
- `src/bot/state.rs`: 增加 `NormalModePosition` (entry_spread, entry_time, accumulated_funding)
- `src/app.rs`: Normal mode主循环重构:
  - 无持仓时: Z > entry_z → 开仓
  - 有持仓时: Z < exit_z || funding adverse → 平仓(maker限价单)
  - 紧急情况: funding突变 → market order平仓

### 关键: 平仓走maker限价单
需要在两个交易所同时挂限价平仓单:
- Binance: BUY limit at bid (maker)
- HL: SELL limit at ask (maker)
- 如果限价单超时未成交 → fallback to market order

### 配置参数
```json
{
  "normal_mode": {
    "spread_window_secs": 1800,
    "entry_z_score": 2.5,
    "exit_z_score": -0.5,
    "max_loss_bps": 30.0,
    "max_position_units": 5.0,
    "close_limit_timeout_secs": 30,
    "funding_poll_interval_secs": 60,
    "funding_settle_buffer_secs": 600,
    "funding_adverse_threshold_bps": 5.0
  }
}
```

## 5. Key Risks

1. **Maker平仓可能不成交**: 限价单可能等很久，需要超时fallback到market
2. **Spread可能不回归**: 结构性价差变化时Z-score会mislead
3. **Funding rate数据延迟**: API polling有间隔，结算前的快速变化可能miss
4. **两边同时挂限价单的执行风险**: 一边成交另一边没成交 → 临时裸仓位

## 6. Data References

- Case 2 spread: `docs/binance_2026-04-14_1330-1530_bjt_spread_history.csv`
- Case 3 spread: `docs/binance_2026-04-14_1940-2026-04-15_0036_bjt_spread_history.csv`
- Case 3 fills: `docs/case_2026-04-14_1940-2026-04-15_0026_bjt/`
- Hyperparameter calibration: `docs/hyperparameter_calibration_2026-04-14.md`

## 7. Funding Rate Data (TODO)

**下一步需先从API获取实际funding数据:**
- Binance CLUSDT: `GET /fapi/v1/premiumIndex?symbol=CLUSDT`
- HL xyz:CL: `POST /info {"type": "fundingHistory", "coin": "xyz:CL"}`
- 确认net carry方向和幅度
- 这决定了funding carry策略的参数设置
