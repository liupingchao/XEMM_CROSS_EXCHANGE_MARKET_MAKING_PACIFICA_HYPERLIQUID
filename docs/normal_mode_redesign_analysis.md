# Normal Mode Redesign: Funding Carry + Spread-Aware Entry

> Analysis date: 2026-04-15. Funding data validated from live APIs.

## 1. Problem: 当前策略只开仓不平仓

每笔交易 SELL Binance + BUY HL 创建跨所 spread position，但**从不平仓**。
Case 3 实盘 28 笔交易：Bot 报告利润 +$2.59，真实平仓后 **净亏 $3.56**。

## 2. Key Discovery: Funding Carry 是主要利润来源

### Funding Rate 实测数据 (2026-04-06 ~ 04-15)

| | Binance CLUSDT | HL xyz:CL |
|---|---|---|
| 结算频率 | **8h** (00/08/16 UTC) | **1h** |
| 平均 rate | -5.30 bps/8h | -2.99 bps/h |
| 折算8h等价 | -5.30 bps | **-23.95 bps** |
| 含义 | 空头每8h付5.3bps | 空头每小时付3bps |
| 对我们的影响 | SHORT付钱 (不利) | LONG收钱 **(有利)** |

### Net Carry (SHORT Binance + LONG HL)

```
HL LONG 收入:     +23.95 bps/8h (每小时收 ~3 bps)
Binance SHORT 支出: -5.30 bps/8h
─────────────────────────────
Net carry:        +17.18 bps/8h = +51.55 bps/day
```

**近10个8h窗口全部盈利 (100%)**，range: +6 ~ +42 bps/8h。

年化 ~188%。Break-even: 建仓后持有 ~10小时即可覆盖全部 round-trip 费用 (8 bps)。

### 价差的根因

HL funding rate 是 Binance 的 4-5 倍。HL 多头收到更多 funding → 愿意接受更低价格 → HL 系统性低于 Binance ~20 bps。**价差是 funding 差异的均衡表现**。

## 3. 策略设计

### 核心思想

**建仓后长期持有吃 carry，不频繁开平仓。** Carry 是确定性收入 (+52 bps/day)，mean-reversion 的波动利润 (+5-15 bps/trade) 扣费后是边缘的。

### 三种市场状态

#### 状态一：常态安静期（spread ~18-22 bps，Z-score < entry 阈值）

**无持仓时**: 不急于开仓，等更好的入场点。持续更新 spread 统计和 funding rate。

**有持仓时**: 持有不动，每小时收 HL funding ~3 bps。这是策略的主要盈利阶段。

#### 状态二：Normal Spike（spread 突然走宽到 25-35 bps，Z > 2.0）

**开仓信号**: 价差偏离均值 → SELL Binance (maker) + BUY HL (taker)

开仓的目的不是做 mean-reversion 短线——而是**在高价差时建仓，降低未来平仓时的 spread 成本**。开仓后默认长期持有吃 carry。

#### 状态三：Event 冲击（spread > event_trigger，50+ bps）

保持现有 event mode 逻辑不变。Event 的 spread 收敛利润 (几十~上百 bps) 远大于 carry，**不管 funding 直接开仓，价差回归后平仓**。

### 平仓条件（仅在以下情况才平仓）

| 条件 | 平仓方式 | 说明 |
|------|----------|------|
| Net carry 连续 N 次为负 | Maker 限价单 | Carry 不划算了 |
| Funding rate 被交易所突然大幅修改 | Market 紧急平仓 | 风控 |
| 价差极端扩大 > max_loss_bps | Market 紧急平仓 | 止损 |
| 持仓超过 max_hold_hours | Maker 限价单 | 定期收割/重置 |
| Spread 偶尔收窄到 < close_spread_bps | Maker 限价单 | 难得的低费用平仓窗口 |

### 双边平仓机制

**Normal 平仓 (maker, 低费用)**:
1. Binance 挂 BUY 限价单 @ best bid (maker 1.5 bps)
2. HL 挂 SELL 限价单 @ best ask (maker ~1.0 bps)
3. 超时未成交 → fallback market order
4. Round-trip: 开仓 5.5 + 平仓 2.5 = **8.0 bps**

**紧急平仓 (taker, 保命)**:
1. 双边 market order 并行发送
2. Round-trip: 开仓 5.5 + 平仓 8.0 = **13.5 bps**

## 4. 完整状态图

```
                        ┌──────────────┐
             ┌─────────→│   空仓 IDLE   │←─────────────────────┐
             │          └──────┬───────┘                       │
             │                 │                               │
             │    Z > entry_z 且 spread > min_entry_spread     │
             │    (等待好的入场价位)                              │
             │                 ▼                               │
             │    ┌────────────────────────┐                   │
             │    │  Normal 持仓            │                   │
             │    │  SHORT BN + LONG HL    │                   │
             │    │                        │                   │
             │    │  主要行为: 持有吃carry   │                   │
             │    │  +17 bps/8h net carry  │                   │
             │    │                        │── carry转负/止损/  │
             │    │  监控:                  │   spread收窄     ─→│
             │    │  - funding rate 每60s   │   (maker平仓)     │
             │    │  - spread (持续)        │                   │
             │    │  - 仓位风险             │── funding突变/     │
             │    └────────────────────────┘   极端风险       ─→│
             │                                 (market紧急平)  │
             │                                                 │
             │    spread > event_trigger (59 bps)              │
             │                 │                               │
             │                 ▼                               │
             │    ┌────────────────────────┐                   │
             │    │  Event 持仓 (不变)      │── spread回归 ────→│
             │    └────────────────────────┘                   │
             └─────────────────────────────────────────────────┘
```

## 5. Implementation Plan

### 新文件

| 文件 | 用途 |
|------|------|
| `src/strategy/spread_stats.rs` | 30 分钟滚动 spread 统计 (用于入场时机判断) |
| `src/strategy/funding.rs` | Funding carry 监控 (net carry, 突变检测) |

### 新 API 方法

| 文件 | 方法 | 用途 |
|------|------|------|
| `src/connector/binance/trading.rs` | `get_premium_index()` | Binance funding rate + 下次结算时间 |
| `src/connector/hyperliquid/trading.rs` | `get_funding_rate()` | HL 最近 funding rate |

### 改造文件

| 文件 | 改动 |
|------|------|
| `src/connector/maker.rs` | trait 增加 `get_funding_info()` |
| `src/config.rs` | normal_mode 增加 carry 策略参数 |
| `src/bot/state.rs` | 增加 `NormalModePosition` |
| `src/app.rs` | Normal mode 主循环: carry-first 持仓管理 |
| `src/strategy/mod.rs` | 导出新模块 |

### 配置参数

```json
{
  "normal_mode": {
    "profit_rate_bps": 5.6,
    "order_notional_usd": 90.0,
    "profit_cancel_threshold_bps": 11.7,
    "order_refresh_interval_secs": 90,
    "entry_z_score": 2.0,
    "spread_window_secs": 1800,
    "max_position_units": 10.0,
    "max_loss_bps": 50.0,
    "max_hold_hours": 168,
    "close_spread_bps": 8.0,
    "funding_poll_interval_secs": 60,
    "funding_adverse_consecutive": 3,
    "funding_adverse_threshold_bps": -2.0,
    "close_limit_timeout_secs": 30
  }
}
```

参数说明:
- `entry_z_score`: 入场 Z-score 阈值 (等 spread spike 时建仓)
- `max_position_units`: 最大累计持仓量
- `max_loss_bps`: 止损阈值 (基于入场 spread vs 当前 spread)
- `max_hold_hours`: 最长持仓时间 (定期收割, 168h = 1周)
- `close_spread_bps`: 当 spread 收窄到此值以下时平仓 (低费用窗口)
- `funding_adverse_consecutive`: net carry 连续为负多少次才触发平仓
- `funding_adverse_threshold_bps`: 什么程度算 "adverse" carry
- `close_limit_timeout_secs`: maker 限价平仓超时后 fallback taker

## 6. Backtest Reference

Carry 策略不需要传统回测 (不频繁开平仓)。核心验证:
- 89 期 Binance funding: 73% 为负 (空头付钱), 平均 -5.3 bps/8h
- 207 期 HL funding: 82% 为负 (我们的多头收钱), 平均 -3.0 bps/h
- **Net carry 近 10 个 8h 窗口 100% 盈利**, +6 ~ +42 bps/8h
- 日均 +52 bps, 27 units 规模 = ~$12.5/day

## 7. Key Risks

| 风险 | 影响 | 缓解 |
|------|------|------|
| Funding 结构性反转 | Carry 变负, 持仓成本 | 连续N次负carry → 自动平仓 |
| HL 修改 funding 机制 | Carry 消失 | funding_adverse 检测 + 紧急平仓 |
| Spread 极端扩大 | 保证金不足 | max_loss_bps 止损 |
| API ban 导致无法平仓 | 仓位锁死 | BinanceApiHealth 健康检测 |
| 长期持仓的保证金占用 | 资金效率 | max_position_units 上限 |
