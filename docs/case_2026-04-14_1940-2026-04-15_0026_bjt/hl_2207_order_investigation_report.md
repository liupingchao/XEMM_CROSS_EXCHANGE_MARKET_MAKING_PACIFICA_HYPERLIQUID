# HL 22:07 可疑平仓单调查报告（Case: 2026-04-14 19:40 ~ 2026-04-15 00:26 BJT）

## 1) 背景与用户反馈（来自两次对话）

### 对话 A（用户反馈）
- 用户在 Binance App 观察到：
  - 21:37（BJT）有卖出订单下达后取消。
  - 21:39（BJT）有买入订单下达后取消。
  - 21:41（BJT）开始出现完全成交的卖出订单。
  - 之后到 00:00（BJT）后，多为“偶尔卖出后取消”，也有“偶尔成交”的卖出订单。
- 用户在 Hyperliquid 观察到：
  - 21:37（BJT）是最后一笔买入。
  - 22:07（BJT）出现一笔约 27.1 WTIOIL 的 `market` 平仓单。
- 用户明确声明：
  - 00:13（BJT）才第一次在binance手动平仓（约 260 CL），和22:07（BJT）这笔 HL `market` 平仓单到数量27.1对不上。
  - 00:13 前没有人工干预。
  - 因此怀疑 22:07（BJT）这笔 HL `market` 平仓单可能由程序触发。

### 对话 B（用户补充）
- 用户提供 HL 订单号：`381138322060`。

---

## 2) 调查目标
- 核实 `oid=381138322060` 是否由当前 XEMM 进程发出。
- 核实该单是否在本次 case 审计链路（log / orders.csv / fills.csv）中可追溯。
- 核实 22:07（BJT）时段是否存在程序内 `exit/rebalance` 自动平仓触发。

---

## 3) 排查范围与证据

### 3.1 本地 case 目录（本次打包证据）
目录：
- `docs/case_2026-04-14_1940-2026-04-15_0026_bjt/`

检查项：
- `xemm_runtime.log`
- `xyz_cl_orders.csv`
- `xyz_cl_fills.csv`
- `hyperliquid_*_orders.csv`
- `hyperliquid_*_fills.csv`

结果：
- 全局检索 `381138322060` 无命中。
- 22:07（BJT = 14:07 UTC）附近日志存在 Binance 下单/撤单活动，但未出现 HL 下单或 `exit_rebalance_market_flatten` 触发日志。

### 3.2 AWS 服务器全盘排查（`awsserver1`）
检查路径：
- `/home/admin/XEMM_rust`
- `/home/admin/XEMM_rust_latest`
- 两目录下 logs / csv 全部文件

结果：
- 全局检索 `381138322060` 无命中。
- 服务器 HL fills 文件中最后记录停在 `oid=381091857727`（2026-04-14T13:37:33Z / 21:37:33 BJT）。
- 未发现与 22:07（BJT）对应的 HL 新 fill 记录。

### 3.3 代码路径核验（是否可能程序主动发 HL market flatten）
代码中确实存在自动平仓路径：
- `src/app.rs` 中 `exit_rebalance_market_flatten` 逻辑会对 HL 发 `reduce_only` 市价单。

但在本次 case 日志中：
- 未检索到 `[EXIT]` / `exit_rebalance_market_flatten` 的实际触发记录。

### 3.4 Hyperliquid 接口反查
针对 `.env` 中 `HL_WALLET=0x1909d71aD99e6F40fEB68A5e0b5bF90988Cd6833`，调用：
- `orderStatus(oid=381138322060)`（mainnet / testnet，含 dex=xyz 尝试）；

结果：
- 返回 `unknownOid`。

额外交叉：
- 对本地 CSV 中已有 HL oid（如 `381091857727`）同样返回 `unknownOid`。
- 这说明“当前查询钱包地址”与“网页订单所在账户”可能并非同一账户视角（或存在账户/子账户视角差异）。

---

## 4) 结论（当前证据下）
1. `oid=381138322060` 不在当前 XEMM 的本地/服务器日志与 CSV 审计链路中。  
2. 22:07（BJT）时段，在当前 case 日志里没有发现程序触发 HL 自动平仓的直接证据。  
3. 通过当前 `HL_WALLET` 查询 HL API，`oid=381138322060` 为 `unknownOid`，提示该单很可能不属于当前 bot 查询的账户视角。  
4. 因此，现有证据不支持“该单由当前这次运行中的 XEMM 进程发出”这一结论。

---

## 5) 待确认项（最关键）
- 需要用户从 HL 网页该订单详情补充：
  - 订单对应 `account/user address`
  - 完整成交时间（带时区）
  - `coin` / `side` / `reduce_only` / `order type`

拿到上述信息后，可做最终归因比对：
- 账户地址是否与 `.env` 中 `HL_WALLET` 一致；
- 时间点是否落在本次进程生命周期；
- 是否能在进程日志中匹配到同时间同方向动作。

---

## 6) 备注
- 本报告只覆盖本次 case 窗口与对应目录证据（2026-04-14 19:40 ~ 2026-04-15 00:26 BJT）及服务器同名运行期文件。
- 若存在其他机器、其他目录、其他 service/进程使用同一 API key 或不同账户，也可能产生“交易所可见但当前 case 无记录”的现象。

## 7）binance和HL仓位在21:37:33 (BJT)之后不匹配原因分析


1. **21:37 之后的限制主要是 Binance 侧**  
- `2026-04-14 20:50:21 (BJT)`：日志出现  
  `BINANCE_FILL ... User stream stopped: Failed after 5 reconnect attempts`  
- `2026-04-14 21:37:53 (BJT)` 起：大量 Binance `418 -1003`（ban）和 `403 Forbidden`，并伴随 REST 限频告警。  
这会导致“真实成交存在，但程序难以及时拿到 fill”。

2. **为什么 HL 21:37 后没有新对冲单**  
- 当前逻辑是“Binance fill -> 触发 HL hedge”。  
- 程序记录里最后一笔 Binance fill 是 `2026-04-14 21:37:32 (BJT)`，最后一笔 HL hedge 是 `2026-04-14 21:37:33 (BJT)`。  
- 所以程序视角下，后续没有新 fill，自然没有新 HL 对冲。

