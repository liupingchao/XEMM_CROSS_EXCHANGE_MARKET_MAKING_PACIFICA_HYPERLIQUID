# Normal Mode Redesign: Mean-Reversion + Funding Carry

> Analysis date: 2026-04-15. To be implemented in next session.

## 1. Problem: 当前策略只开仓不平仓

每笔交易 SELL Binance + BUY HL 创建跨所 spread position，但**从不平仓**。

Case 3 实盘 28 笔交易：Bot 报告利润 +$2.59，但如果真正平仓需付平仓价差 $4.22 + 费用 $1.93 = **净亏 $3.56**。原因是入场赚到的 15.9 bps 价差和平仓要付出的 20 bps 价差是同一个结构性价差——开仓不等于获利了结。

## 2. 新策略：完整开平仓闭环

### 市场背景

CLUSDT (Binance) 持续比 xyz:CL (Hyperliquid) 贵约 20 bps，这个价差围绕均值波动（std 3-8 bps），偶尔因新闻冲击飙升到 50-230 bps。两边 funding 每 8 小时结算一次。

### 费用结构（决定策略是否可行的关键）

| | 开仓 | 平仓 (maker限价) | 平仓 (taker市价) |
|---|---|---|---|
| Binance | maker 1.5 bps | maker 1.5 bps | taker 4.0 bps |
| HL | taker 4.0 bps | maker ~1.0 bps | taker 4.0 bps |
| **单边合计** | **5.5 bps** | **2.5 bps** | **8.0 bps** |
| **Round-trip** | — | **8.0 bps** | **13.5 bps** |

**核心洞察**：平仓走 maker 限价单 (8.0 bps RT) 和走 taker 市价单 (13.5 bps RT) 差 5.5 bps。这 5.5 bps 决定了策略能否盈利。

---

## 3. 三种市场状态下的行为

### 状态一：常态安静期（价差 ~20 bps，波动很小）

```
价差在 18-22 bps 之间窄幅波动，Z-score 在 ±1 之间
```

**行为：不交易，等待。**

价差波动幅度 (~3-5 bps) 小于 round-trip 费用 (8 bps)，即使平仓走 maker 也不够覆盖。此时 bot idle，只做两件事：
1. 持续更新 30 分钟滚动窗口的价差均值和标准差
2. 每 60 秒查询两边 funding rate

如果已有持仓（从之前的 spike 留下来的）且 net funding carry > 0（Binance 空头收 funding > HL 多头付 funding），则**继续持有**，靠 carry 赚钱，不急于平仓。

### 状态二：中等波动 / Normal Spike（价差突然走宽到 25-35 bps）

```
价差从 20 bps 突然跳到 28 bps → Z-score 升到 2.5 以上
典型触发：小新闻、流动性瞬时枯竭、大单冲击
持续时间：几秒到几分钟
```

**开仓条件**（无持仓时）：
```
Z-score > 2.5（价差偏离均值 2.5 个标准差以上）
且 当前价差 > 8 bps（覆盖 round-trip 费用）
```
→ SELL Binance (maker 限价单) + fill 后 BUY HL (taker 市价单对冲)
→ 记录入场价差 entry_spread_bps

**平仓条件**（有持仓时）：
```
Z-score < -0.5（价差回归到均值以下半个标准差）
即：价差从 28 → 回落到 ~18 bps
```
→ **双边 maker 限价单平仓**（见下方平仓机制）

**利润计算**（以回测中位数为例）：
```
开仓时价差: 25 bps    平仓时价差: 18 bps
价差收敛利润: 25 - 18 = 7 bps
减去 round-trip 费用: -8 bps
净 P&L: -1 bps（勉强打平）

但如果开仓价差 28 bps，平仓 17 bps:
价差收敛: 11 bps - 费用 8 bps = +3 bps（盈利）
```

回测结论：`entry_z=2.5, exit_z=-0.5` 在 Case 2 和 Case 3 上均盈利（+23 / +19 bps）。更高的 entry_z=3.0 胜率 75%。

**Funding 加成**：
- 持仓期间如果 net carry > 0，放宽平仓条件（exit_z 从 -0.5 降到 -1.0），让仓位多持一会收 funding
- 持仓期间如果 net carry < 0 且接近结算（< 10 分钟），收紧平仓条件，尽快平仓避免付 funding

### 状态三：Event 冲击（价差飙升到 50-230 bps）

```
重大新闻、WTI 暴跌/暴涨
HL 流动性枯竭，价格快速偏离 Binance
价差从 20 bps → 100-230 bps
持续时间：数分钟到数十分钟
```

**开仓：不管 funding，立即开仓。**

Event 的价差收敛利润（几十到上百 bps）远大于一次 funding 结算的影响（几个 bps）。先吃大肉。

→ SELL Binance (maker 限价单，用 event_mode 参数) + BUY HL (taker 市价)
→ 可以连续开多笔（在 v2-fast-cycle 版本下，cycle 间隔缩短到 ~5s）

**平仓条件**：
```
1. 价差回落到 exit_rebalance_exit_spread_bps（如 21 bps，正常态 P75）
   → maker 限价单平仓（低费用）

2. Funding rate 被交易所突然大幅修改（如从 +0.01% 跳到 -0.1%）
   → market order 紧急平仓（高费用但避免更大损失）
```

Event 模式的利润空间大，即使平仓用 taker (13.5 bps RT) 也盈利。但如果时间允许，仍然优先用 maker 限价单平仓节省费用。

---

## 4. 双边平仓机制（三种场景）

### 场景 A：Normal 平仓（maker 限价，低费用）

```
触发条件: Z-score 回归 或 funding carry 条件触发
时间压力: 低（可以等）
```

执行流程：
1. 在 Binance 挂 **BUY 限价单 @ best bid**（maker，1.5 bps）
2. 在 HL 挂 **SELL 限价单 @ best ask**（maker，~1 bps）
3. 监控两边成交状态，每秒检查
4. 如果一边成交、另一边 **30 秒内**未成交 → 把未成交的一边转为 market order
5. 如果两边都 **30 秒内**未成交 → 判断价差是否重新走宽，如果走宽则取消平仓（继续持有），如果价差依然在 exit 区间则重新调价

费用：2.5 bps（最优）到 5.5-6.5 bps（一边成交一边 fallback）

### 场景 B：Event 回归平仓（maker 限价，有时间但价差在移动）

```
触发条件: Event 模式下价差从高位回落到正常区间
时间压力: 中（价差在快速变化，但方向有利）
```

执行流程：
1. 同场景 A，但 timeout 缩短到 **10 秒**
2. 如果超时 → 立即 taker market order 两边平仓
3. 宁可多付 5.5 bps 费用也不要错过价差收敛窗口

费用：2.5 bps（理想）到 8.0 bps（全 taker fallback）

### 场景 C：紧急平仓（market order，保命）

```
触发条件:
  - Funding rate 被交易所突然大幅修改
  - 止损触发（价差反向扩大 > max_loss_bps）
  - API 健康状态恶化（Binance ban / HL down）
时间压力: 极高
```

执行流程：
1. Binance **BUY market order** (taker, 4.0 bps)
2. HL **SELL market order** (taker, 4.0 bps)
3. 两边并行发送，不等一边成交再发另一边

费用：8.0 bps（固定，无法优化）

---

## 5. 持仓期间的 Funding Carry 逻辑

```
持仓结构: Binance SHORT + HL LONG
Funding 机制: 每 8h 结算 (00:00, 08:00, 16:00 UTC)

当 funding rate > 0（多数期货合约常态）:
  Binance SHORT 收到 funding  ← 有利
  HL LONG 支付 funding        ← 不利
  Net carry = Binance收入 - HL支出

当 net carry > 0:
  持仓有正收益 → 放宽平仓条件，让仓位多持
  即使 Z-score 已回归到均值，也可以继续持有

当 net carry < 0:
  持仓有负成本 → 收紧平仓条件
  如果距离下次结算 < 10 分钟 → 强制平仓避免付 funding

当 funding rate 被交易所突然修改:
  每 60 秒 poll 一次 funding rate
  如果 rate 变化幅度 > funding_adverse_threshold_bps → 紧急平仓 (场景 C)
```

---

## 6. 回测验证（用 maker close 8.0 bps RT）

### Case 2（最稳定的 2h 午后数据）

| entry_z | exit_z | 交易数 | 胜率 | 总 P&L |
|---------|--------|--------|------|--------|
| 2.0 | -0.5 | 40 | 35% | -10.8 bps |
| **2.5** | **-0.5** | **24** | **46%** | **+23.2 bps** |
| **3.0** | **-0.5** | **12** | **75%** | **+28.3 bps** |
| 3.0 | 0.0 | 12 | 58% | +13.0 bps |

### Case 3（4.9h 实盘数据，含 WTI 暴跌）

| entry_z | exit_z | 交易数 | 胜率 | 总 P&L |
|---------|--------|--------|------|--------|
| 2.0 | -0.5 | 36 | 36% | +14.1 bps |
| **2.5** | **-0.5** | **25** | **44%** | **+18.6 bps** |
| **3.0** | **-0.5** | **12** | **75%** | **+56.0 bps** |

注：以上 P&L 未包含 funding carry 收益。如果 net carry 为正，实际收益更高。

---

## 7. 完整状态图

```
                        ┌──────────────┐
             ┌─────────→│   空仓 IDLE   │←──────────────────────┐
             │          └──────┬───────┘                        │
             │                 │                                │
             │    Z > 2.5 且 spread > 8 bps                    │
             │    (normal spike)                                │
             │                 │              Z < exit_z         │
             │                 ▼              或 carry条件       │
             │    ┌────────────────────────┐     │              │
             │    │  Normal 持仓            │─────┘              │
             │    │  (short BN + long HL)  │                    │
             │    │  监控: Z-score + funding│──── maker平仓 ────→│
             │    └────────────────────────┘     (场景A)        │
             │                                                  │
             │    spread > event_trigger (59 bps)               │
             │    (event冲击)                                    │
             │                 │                                │
             │                 ▼              spread回落         │
             │    ┌────────────────────────┐     │              │
             │    │  Event 持仓            │─────┘              │
             │    │  (short BN + long HL)  │                    │
             │    │  更大仓位，不管funding  │──── maker平仓 ────→│
             │    └─────────┬──────────────┘     (场景B)        │
             │              │                                   │
             │    funding突变 / 止损                              │
             │              │                                   │
             │              └────────── market紧急平仓 ────────→│
             │                              (场景C)             │
             │                                                  │
             └──────────────────────────────────────────────────┘
```

---

## 8. Implementation Plan (Next Session)

### 新文件
| 文件 | 用途 |
|------|------|
| `src/strategy/spread_stats.rs` | 30 分钟滚动窗口的均值/标准差/Z-score 计算 |
| `src/strategy/funding.rs` | Funding carry 计算 (net carry, 距结算时间, 突变检测) |

### 新 API 方法
| 文件 | 方法 | 用途 |
|------|------|------|
| `src/connector/binance/trading.rs` | `get_premium_index()` | Binance funding rate + 下次结算时间 |
| `src/connector/hyperliquid/trading.rs` | `get_funding_rate()` | HL funding rate + 历史 |

### 改造文件
| 文件 | 改动 |
|------|------|
| `src/connector/maker.rs` | trait 增加 `get_funding_info()` + `place_limit_close_order()` |
| `src/config.rs` | `ModeTradingConfig` 增加 Z-score / funding / 平仓超时参数 |
| `src/bot/state.rs` | 增加 `NormalModePosition` (entry_spread, size, accumulated_funding) |
| `src/app.rs` | Normal mode 主循环重构: Z-score 开仓 / 平仓决策 + funding 监控 |

### 配置参数
```json
{
  "normal_mode": {
    "profit_rate_bps": 5.6,
    "order_notional_usd": 90.0,
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

### 前置步骤
**先从 API 获取实际 funding 数据，确认 net carry 方向和幅度：**
- Binance: `GET /fapi/v1/premiumIndex?symbol=CLUSDT`
- HL: `POST /info {"type": "fundingHistory", "coin": "xyz:CL"}`

---

## 9. Key Risks

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| Maker 平仓不成交 | 仓位滞留，价差可能反转 | 30 秒超时后 fallback taker |
| 一边成交另一边没成交 | 临时裸仓位 | 未成交侧立即 taker 补单 |
| Spread 结构性偏移 | Z-score 误判 | max_loss_bps 止损兜底 |
| Funding rate 数据延迟 | 错过结算前调仓 | 提前 10 分钟开始监控 |
| API ban 导致平仓失败 | 仓位无法平 | P0 修复已加 BinanceApiHealth，ban 时暂停开仓 |
