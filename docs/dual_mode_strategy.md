# XEMM 双模式策略（Normal + Event）

## 1. 目标

在保留原有常态价差策略（Normal）的基础上，增加事件冲击策略（Event），形成双模式：

1. 常态阶段：低噪音、稳定小边际的持续做市/价差。
2. 事件阶段：检测到流动性冲击后，切换到事件参数，捕捉价差回归。
3. 回归阶段：价差回落后触发 flatten/rebalance，两边仓位回到 0，再回常态。

## 2. 策略思想

### 2.1 Normal 模式

1. 使用较稳健参数（较低目标利润、较高成交概率）。
2. 按现有 `OpportunityEvaluator` 持续评估并下单。
3. 撤单阈值和刷新间隔偏保守，控制撤单抖动与手续费磨损。

### 2.2 Event 模式

1. 事件触发条件：
   - `|mid_spread_bps|` 超过阈值；
   - 且 `hl_spread_bps`（Hyperliquid 自身买卖价差）也超过阈值；
   - 持续满足 `confirm_secs` 后才进入事件模式。
2. 方向约束：
   - `mid_spread_bps > 0`：maker 相对更贵，优先 `SELL maker`；
   - `mid_spread_bps < 0`：maker 相对更便宜，优先 `BUY maker`。
3. 退出逻辑：
   - 事件模式下，当 `|mid_spread_bps|` 回落到 exit 阈值并确认后，触发 `exit_rebalance`。
   - flatten 完成后进入 rearm cooldown，再允许下一次事件触发。

## 3. 状态机

1. `Normal`：常态参数运行。
2. `EventActive`：事件参数运行，且限制入场方向。
3. `ExitPending`：事件回落确认阶段，暂停新开仓，等待 flatten。
4. `Cooldown`：事件结束后的短冷却，防止反复切换。

说明：当前实现里 `ExitPending/Cooldown` 由 `RegimeController + exit_rebalance` 的组合行为体现，不额外引入复杂的全局状态对象。

## 4. 代码改造点

### 4.1 配置层

文件：`src/config.rs`

新增：

1. `StrategyMode`：`normal | event_only | dual`
2. `ModeTradingConfig`：
   - `profit_rate_bps`
   - `order_notional_usd`
   - `profit_cancel_threshold_bps`
   - `order_refresh_interval_secs`
3. `normal_mode` / `event_mode`（可选覆盖）
4. 事件触发参数：
   - `event_trigger_mid_spread_bps`
   - `event_trigger_hl_spread_bps`
   - `event_trigger_confirm_secs`
   - `event_rearm_cooldown_secs`
5. `continuous_mode`：是否连续运行（不再单循环结束后退出）

兼容性：保留旧字段（`profit_rate_bps` 等）作为 fallback。

### 4.2 策略层

文件：`src/strategy/regime.rs`（新增）

新增 `RegimeController`：

1. 维护事件候选确认计时。
2. 输出当前应使用的 profile（Normal/Event）。
3. 在 Event 模式下给出方向过滤（只允许一侧方向）。
4. 支持事件结束后 cooldown。

### 4.3 主循环

文件：`src/app.rs`

改造：

1. 同时持有 `normal_evaluator` 和 `event_evaluator`。
2. 每个 tick 先计算：
   - `mid_spread_bps`
   - `hl_spread_bps`
3. 交给 `RegimeController` 决策 profile。
4. 若 Event 且满足回落条件，触发 `trigger_exit_rebalance()`。
5. 下单时根据 profile 选择：
   - evaluator
   - notional
   - 订单日志 source（`main_loop_limit_order_normal/event`）

### 4.4 订单监控

文件：`src/services/order_monitor.rs`

改造：

1. `OrderSnapshot` 增加：
   - `profit_cancel_threshold_bps`
   - `order_refresh_interval_secs`
2. 监控逻辑改为读取快照阈值（订单级别），避免 Event 单被 Normal 阈值误撤。

### 4.5 Hedge 生命周期

文件：`src/services/hedge.rs`、`src/bot/state.rs`

改造：

1. 新增 `mark_cycle_complete_continue()`。
2. `continuous_mode = true` 时：成功对冲后回到 `Idle`，继续跑。
3. `continuous_mode = false` 时：保持原行为（Complete + shutdown）。

## 5. 上线顺序

1. 本地 `cargo check` + 关键路径回归（下单、fill、hedge、exit_rebalance）。
2. 小资金实盘（1 unit）跑 2-4 小时观察。
3. 重点看日志：
   - regime 切换是否符合预期
   - event 触发后是否只走单方向
   - exit_rebalance 后是否回到双边接近 0
4. 再做 24 小时 soak。

## 6. 风险与限制

1. 事件识别依赖固定阈值，后续可加动态分位数阈值（rolling quantile）。
2. Event 期间如果交易所异常，仍可能出现不对称成交，需要强制风控兜底。
3. 当前为单对 symbol 设计，后续多 symbol 建议把 `RegimeController` 下沉到 symbol 维度。

