use anyhow::{Context, Result};
use colored::Colorize;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use parking_lot::Mutex;
use std::time::{Duration, Instant};
use tokio::signal;
use tokio::sync::{mpsc, RwLock};
use tokio::time::interval;
use tracing::{debug, info};

use crate::audit_logger;
use crate::bot::{ActiveOrder, BotState, BotStatus};
use crate::config::{Config, ModeTradingConfig};
use crate::connector::binance::{BinanceCredentials, BinanceTrading};
use crate::connector::hyperliquid::{HyperliquidCredentials, HyperliquidTrading};
use crate::connector::maker::{
    BinanceMakerExchange, MakerExchange, MakerExchangeKind, MakerOrderSide, PacificaMakerExchange,
};
use crate::connector::pacifica::{
    FillDetectionClient, FillDetectionConfig, PacificaCredentials, PacificaTrading,
    PacificaWsTrading,
};
use crate::services::{
    binance_fill_detection::BinanceFillDetectionService,
    fill_detection::FillDetectionService, hedge::HedgeService,
    order_monitor::{AtomicBotStatus, OrderMonitorService, SharedOrderSnapshot, spawn_monitor_tasks, sync_atomic_status, update_order_snapshot},
    orderbook::{HyperliquidOrderbookService, PacificaOrderbookService},
    position_monitor::PositionMonitorService, rest_fill_detection::RestFillDetectionService,
    rest_poll::{HyperliquidRestPollService, MakerRestPollService},
    spread_recorder::SpreadRecorderService,
    HedgeEvent,
};
use crate::strategy::{EntryProfile, OpportunityEvaluator, OrderSide, RegimeController, RegimeSettings};
use crate::util::rate_limit::{is_rate_limit_error, RateLimitTracker};

fn is_post_only_reject_error(error: &anyhow::Error) -> bool {
    let s = error.to_string().to_ascii_lowercase();
    s.contains("-5022")
        || s.contains("post only")
        || s.contains("post-only")
        || s.contains("executed as maker")
}

fn maker_side_to_strategy_side(side: MakerOrderSide) -> OrderSide {
    match side {
        MakerOrderSide::Buy => OrderSide::Buy,
        MakerOrderSide::Sell => OrderSide::Sell,
    }
}



/// Position snapshot for tracking position deltas
#[derive(Debug, Clone)]
pub struct PositionSnapshot {
    pub amount: f64,
    pub side: String, // "bid" or "ask"
    pub last_check: Instant,
}

/// XemmBot - Main application structure that encapsulates all bot components
pub struct XemmBot {
    pub config: Config,
    pub bot_state: Arc<RwLock<BotState>>,

    // Maker exchange backend (abstraction for Pacifica/Binance/...)
    pub maker_kind: MakerExchangeKind,
    pub maker_exchange: Arc<dyn MakerExchange>,

    // Optional Pacifica-specific clients (used only when maker = Pacifica)
    pub pacifica_trading_fill: Option<Arc<PacificaTrading>>,
    pub pacifica_ws_trading: Option<Arc<PacificaWsTrading>>,
    pub hyperliquid_trading: Arc<HyperliquidTrading>,

    // Shared state (prices)
    pub pacifica_prices: Arc<Mutex<(f64, f64)>>, // (bid, ask)
    pub hyperliquid_prices: Arc<Mutex<(f64, f64)>>, // (bid, ask)

    // Opportunity evaluator
    pub normal_evaluator: OpportunityEvaluator,
    pub event_evaluator: OpportunityEvaluator,
    pub normal_mode: ModeTradingConfig,
    pub event_mode: ModeTradingConfig,

    // Fill tracking state
    pub processed_fills: Arc<parking_lot::Mutex<HashSet<String>>>,
    pub last_position_snapshot: Arc<parking_lot::Mutex<Option<PositionSnapshot>>>,

    // Order monitor state (lock-free)
    pub atomic_status: Arc<AtomicU8>,
    pub order_snapshot: Arc<SharedOrderSnapshot>,

    // Channels
    pub hedge_tx: mpsc::UnboundedSender<HedgeEvent>,
    pub hedge_rx: Option<mpsc::UnboundedReceiver<HedgeEvent>>,
    pub shutdown_tx: mpsc::Sender<()>,
    pub shutdown_rx: Option<mpsc::Receiver<()>>,

    // Credentials (needed for spawning Pacifica-only services)
    pub pacifica_credentials: Option<PacificaCredentials>,

    // Hyperliquid wallet address (validated at startup)
    pub hl_wallet: String,
}

impl XemmBot {
    /// Create and initialize a new XemmBot instance
    ///
    /// This performs all the wiring:
    /// - Loads config and validates it
    /// - Loads credentials from environment
    /// - Creates all trading clients
    /// - Pre-fetches Hyperliquid metadata
    /// - Cancels existing orders
    /// - Fetches maker tick size
    /// - Creates OpportunityEvaluator
    /// - Initializes shared state and channels
    pub async fn new() -> Result<Self> {
        use colored::Colorize;

        info!("{}", "═══════════════════════════════════════════════════"
                .bright_cyan()
                .bold()
        );
        info!("{}", "  XEMM Bot - Cross-Exchange Market Making"
                .bright_cyan()
                .bold()
        );
        info!("{}", "═══════════════════════════════════════════════════"
                .bright_cyan()
                .bold()
        );
        info!("");

        // Load configuration
        let config = Config::load_default().context("Failed to load config.json")?;
        config.validate().context("Invalid configuration")?;
        let maker_symbol = config.maker_symbol_str().to_string();
        let hedge_symbol = config.hedge_symbol_str().to_string();
        let normal_mode = config.normal_mode_config();
        let event_mode = config.event_mode_config();

        info!("{} Symbol Pair: {} -> {}",
            "[CONFIG]".blue().bold(),
            maker_symbol.bright_white().bold(),
            hedge_symbol.bright_white().bold()
        );
        info!("{} Strategy Mode: {:?}",
            "[CONFIG]".blue().bold(),
            config.strategy_mode
        );
        info!("{} Pacifica Maker Fee: {}",
            "[CONFIG]".blue().bold(),
            format!("{} bps", config.pacifica_maker_fee_bps).bright_white()
        );
        info!("{} Hyperliquid Taker Fee: {}",
            "[CONFIG]".blue().bold(),
            format!("{} bps", config.hyperliquid_taker_fee_bps).bright_white()
        );
        info!("{} Normal Profile: notional=${:.2}, profit={:.2}bps, cancel={:.2}bps, refresh={}s",
            "[CONFIG]".blue().bold(),
            normal_mode.order_notional_usd,
            normal_mode.profit_rate_bps,
            normal_mode.profit_cancel_threshold_bps,
            normal_mode.order_refresh_interval_secs
        );
        info!("{} Event Profile: notional=${:.2}, profit={:.2}bps, cancel={:.2}bps, refresh={}s",
            "[CONFIG]".blue().bold(),
            event_mode.order_notional_usd,
            event_mode.profit_rate_bps,
            event_mode.profit_cancel_threshold_bps,
            event_mode.order_refresh_interval_secs
        );
        info!("{} Event Trigger: |mid|>={:.2}bps, hl_spread>={:.2}bps, confirm={}s, rearm={}s",
            "[CONFIG]".blue().bold(),
            config.event_trigger_mid_spread_bps,
            config.event_trigger_hl_spread_bps,
            config.event_trigger_confirm_secs,
            config.event_rearm_cooldown_secs
        );
        info!("{} Continuous Mode: {}",
            "[CONFIG]".blue().bold(),
            if config.continuous_mode {
                "enabled".green().bold().to_string()
            } else {
                "disabled".yellow().bold().to_string()
            }
        );
        info!("{} Evaluation Loop Interval: {}",
            "[CONFIG]".blue().bold(),
            format!("{} ms", config.evaluation_loop_interval_ms).bright_white()
        );
        info!("{} Order Failure Cooldown: {}",
            "[CONFIG]".blue().bold(),
            format!("{} ms", config.order_failure_cooldown_ms).bright_white()
        );
        info!("{} Post-only Reject Cooldown: {}",
            "[CONFIG]".blue().bold(),
            format!("{} ms", config.post_only_reject_cooldown_ms).bright_white()
        );
        info!("{} Exit/Rebalance Trigger: {}",
            "[CONFIG]".blue().bold(),
            if config.enable_exit_rebalance {
                "enabled".green().bold().to_string()
            } else {
                "disabled".bright_black().to_string()
            }
        );
        info!("{} Exit/Rebalance Thresholds: arm={} bps, exit={} bps, confirm={}s",
            "[CONFIG]".blue().bold(),
            format!("{:.2}", config.exit_rebalance_entry_spread_bps).bright_white(),
            format!("{:.2}", config.exit_rebalance_exit_spread_bps).bright_white(),
            config.exit_rebalance_confirm_secs
        );
        info!("{} Exit/Rebalance Position Floor: {} | Cooldown: {}s",
            "[CONFIG]".blue().bold(),
            format!("{:.4}", config.exit_rebalance_min_abs_position).bright_white(),
            config.exit_rebalance_cooldown_secs
        );
        info!("{} Pacifica REST Poll Interval: {}",
            "[CONFIG]".blue().bold(),
            format!("{} secs", config.pacifica_rest_poll_interval_secs).bright_white()
        );
        info!("{} Active Order REST Poll Interval: {}",
            "[CONFIG]".blue().bold(),
            format!("{} ms", config.pacifica_active_order_rest_poll_interval_ms).bright_white()
        );
        info!("{} Hyperliquid Market Order maximum allowed Slippage: {}",
            "[CONFIG]".blue().bold(),
            format!("{}%", config.hyperliquid_slippage * 100.0).bright_white()
        );
        info!("");

        // Load credentials
        dotenv::dotenv().ok();
        let hyperliquid_credentials =
            HyperliquidCredentials::from_env().context("Failed to load Hyperliquid credentials from environment")?;

        let maker_name = config.maker_exchange.to_ascii_lowercase();
        let (maker_kind, maker_exchange, pacifica_trading_fill, pacifica_ws_trading, pacifica_credentials): (
            MakerExchangeKind,
            Arc<dyn MakerExchange>,
            Option<Arc<PacificaTrading>>,
            Option<Arc<PacificaWsTrading>>,
            Option<PacificaCredentials>,
        ) = match maker_name.as_str() {
            "pacifica" => {
                let creds = PacificaCredentials::from_env()
                    .context("Failed to load Pacifica credentials from environment")?;
                let pacifica_trading_main = Arc::new(
                    PacificaTrading::new(creds.clone())
                        .context("Failed to create Pacifica trading client")?,
                );
                let pacifica_trading_fill = Arc::new(
                    PacificaTrading::new(creds.clone())
                        .context("Failed to create Pacifica fill trading client")?,
                );
                let maker_exchange: Arc<dyn MakerExchange> =
                    Arc::new(PacificaMakerExchange::new(pacifica_trading_main));
                let ws = Arc::new(PacificaWsTrading::new(creds.clone(), false));
                (
                    MakerExchangeKind::Pacifica,
                    maker_exchange,
                    Some(pacifica_trading_fill),
                    Some(ws),
                    Some(creds),
                )
            }
            "binance" => {
                let creds = BinanceCredentials::from_env()
                    .context("Failed to load Binance credentials from environment")?;
                let binance_trading = Arc::new(
                    BinanceTrading::new(creds, false)
                        .context("Failed to create Binance trading client")?,
                );
                let maker_exchange: Arc<dyn MakerExchange> =
                    Arc::new(BinanceMakerExchange::new(binance_trading));
                (
                    MakerExchangeKind::Binance,
                    maker_exchange,
                    None,
                    None,
                    None,
                )
            }
            _ => unreachable!("config.validate() already ensures valid maker_exchange"),
        };

        let hl_wallet = hyperliquid_credentials.wallet.clone();
        let hyperliquid_trading = Arc::new(
            HyperliquidTrading::new(hyperliquid_credentials, false)
                .context("Failed to create Hyperliquid trading client")?,
        );

        info!(
            "{} {} Maker={} / Hedge=Hyperliquid",
            "[INIT]".cyan().bold(),
            "Trading clients initialized.".green(),
            maker_exchange.name().bright_white().bold(),
        );

        // Pre-fetch Hyperliquid metadata (szDecimals, etc.) to reduce hedge latency
        info!("{} Pre-fetching Hyperliquid metadata for {}...",
            "[INIT]".cyan().bold(),
            hedge_symbol.bright_white()
        );
        hyperliquid_trading
            .get_meta()
            .await
            .context("Failed to pre-fetch Hyperliquid metadata")?;
        info!("{} {} Hyperliquid metadata cached",
            "[INIT]".cyan().bold(),
            "✓".green().bold()
        );

        // Cancel any existing orders on maker exchange at startup
        info!("{} Cancelling any existing orders on {}...",
            "[INIT]".cyan().bold(),
            maker_exchange.name()
        );
        match maker_exchange.cancel_all_orders(Some(&maker_symbol)).await {
            Ok(count) => info!("{} {} Cancelled {} existing order(s)",
                "[INIT]".cyan().bold(),
                "✓".green().bold(),
                count
            ),
            Err(e) => info!("{} {} Failed to cancel existing orders: {}",
                "[INIT]".cyan().bold(),
                "⚠".yellow().bold(),
                e
            ),
        }

        // Get maker symbol info to determine tick size
        let maker_symbol_info = maker_exchange
            .get_symbol_info(&maker_symbol)
            .await
            .with_context(|| format!("Failed to fetch {} symbol info", maker_exchange.name()))?;
        let maker_tick_size = maker_symbol_info.tick_size;

        info!("{} {} tick size for {}: {}",
            "[INIT]".cyan().bold(),
            maker_exchange.name(),
            maker_symbol.bright_white(),
            format!("{}", maker_tick_size).bright_white()
        );

        // Create opportunity evaluators for dual-mode profiles
        let normal_evaluator = OpportunityEvaluator::new(
            config.pacifica_maker_fee_bps,
            config.hyperliquid_taker_fee_bps,
            normal_mode.profit_rate_bps,
            maker_tick_size,
        );
        let event_evaluator = OpportunityEvaluator::new(
            config.pacifica_maker_fee_bps,
            config.hyperliquid_taker_fee_bps,
            event_mode.profit_rate_bps,
            maker_tick_size,
        );

        info!("{} {}",
            "[INIT]".cyan().bold(),
            "Opportunity evaluator created".green()
        );

        // Shared state for orderbook prices
        let pacifica_prices = Arc::new(Mutex::new((0.0, 0.0))); // (bid, ask)
        let hyperliquid_prices = Arc::new(Mutex::new((0.0, 0.0))); // (bid, ask)

        // Shared bot state
        let bot_state = Arc::new(RwLock::new(BotState::new()));

        // Channels for communication
        // Unbounded hedge event queue: producers never block when enqueueing,
        // hedge executor processes events sequentially.
        let (hedge_tx, hedge_rx) = mpsc::unbounded_channel::<HedgeEvent>(); // (side, size, avg_price, fill_timestamp)
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        // Fill tracking state
        let processed_fills = Arc::new(parking_lot::Mutex::new(HashSet::<String>::new()));
        let last_position_snapshot = Arc::new(parking_lot::Mutex::new(Option::<PositionSnapshot>::None));

        info!("{} {}",
            "[INIT]".cyan().bold(),
            "State and channels initialized".green()
        );
        info!("");

        // Initialize order monitor state
        let atomic_status = Arc::new(AtomicU8::new(AtomicBotStatus::Idle as u8));
        let order_snapshot = Arc::new(SharedOrderSnapshot::new());

        Ok(XemmBot {
            config,
            bot_state,
            maker_kind,
            maker_exchange,
            pacifica_trading_fill,
            pacifica_ws_trading,
            hl_wallet,
            hyperliquid_trading,
            pacifica_prices,
            hyperliquid_prices,
            normal_evaluator,
            event_evaluator,
            normal_mode,
            event_mode,
            processed_fills,
            last_position_snapshot,
            atomic_status,
            order_snapshot,
            hedge_tx,
            hedge_rx: Some(hedge_rx),
            shutdown_tx,
            shutdown_rx: Some(shutdown_rx),
            pacifica_credentials,
        })
    }

    async fn fetch_signed_positions(
        &self,
        maker_symbol: &str,
        hedge_symbol: &str,
    ) -> Result<(f64, f64)> {
        let maker_positions = self
            .maker_exchange
            .get_positions(Some(maker_symbol))
            .await
            .with_context(|| format!("Failed to fetch {} positions", self.maker_exchange.name()))?;
        let maker_signed = maker_positions
            .iter()
            .map(|p| p.signed_amount())
            .sum::<f64>();

        let hl_wallet =
            std::env::var("HL_WALLET").unwrap_or_else(|_| self.hyperliquid_trading.get_wallet_address());
        let user_state = self
            .hyperliquid_trading
            .get_user_state(&hl_wallet)
            .await
            .context("Failed to fetch Hyperliquid user state for exit/rebalance")?;
        let hedge_signed = user_state
            .asset_positions
            .iter()
            .find(|ap| ap.position.coin == hedge_symbol)
            .and_then(|ap| ap.position.szi.parse::<f64>().ok())
            .unwrap_or(0.0);

        Ok((maker_signed, hedge_signed))
    }

    async fn trigger_exit_rebalance(
        &self,
        maker_symbol: &str,
        hedge_symbol: &str,
        pac_bid: f64,
        pac_ask: f64,
        hl_bid: f64,
        hl_ask: f64,
        abs_mid_spread_bps: f64,
    ) -> Result<bool> {
        let min_abs_pos = self.config.exit_rebalance_min_abs_position.max(0.0);
        let (maker_pos, hedge_pos) = self
            .fetch_signed_positions(maker_symbol, hedge_symbol)
            .await?;

        if maker_pos.abs() < min_abs_pos && hedge_pos.abs() < min_abs_pos {
            info!(
                "[EXIT] Triggered at {:.2} bps but already near flat (maker={:.6}, hedge={:.6}, floor={:.6})",
                abs_mid_spread_bps,
                maker_pos,
                hedge_pos,
                min_abs_pos
            );
            return Ok(false);
        }

        info!(
            "[EXIT] Triggered at {:.2} bps, flattening positions | maker={:.6}, hedge={:.6}",
            abs_mid_spread_bps,
            maker_pos,
            hedge_pos
        );

        if let Err(e) = self.maker_exchange.cancel_all_orders(Some(maker_symbol)).await {
            info!(
                "[EXIT] {} Failed to cancel maker orders before flatten: {}",
                "⚠".yellow().bold(),
                e
            );
        }

        if maker_pos.abs() >= min_abs_pos {
            let maker_side = if maker_pos > 0.0 {
                MakerOrderSide::Sell
            } else {
                MakerOrderSide::Buy
            };
            let maker_size = maker_pos.abs();
            let maker_price_hint = if matches!(maker_side, MakerOrderSide::Buy) {
                pac_ask
            } else {
                pac_bid
            };

            let maker_order = self
                .maker_exchange
                .place_market_order(maker_symbol, maker_side, maker_size, true)
                .await
                .with_context(|| {
                    format!(
                        "Failed maker exit market order ({} {} {})",
                        self.maker_exchange.name(),
                        maker_side.as_str(),
                        maker_size
                    )
                })?;

            let order_csv = audit_logger::order_file_for_symbol(maker_symbol);
            let order_record = audit_logger::OrderRecord::new(
                chrono::Utc::now(),
                maker_symbol.to_string(),
                self.maker_exchange.name().to_string(),
                maker_side_to_strategy_side(maker_side),
                maker_size,
                maker_price_hint.max(0.0),
                maker_order.order_id.clone(),
                maker_order.client_order_id.clone(),
                "exit_rebalance_market_flatten".to_string(),
            );
            if let Err(e) = audit_logger::log_order(&order_csv, &order_record) {
                info!("[EXIT] {} Failed to log maker exit order: {}", "⚠".yellow().bold(), e);
            }
        }

        if hedge_pos.abs() >= min_abs_pos {
            let hl_is_buy = hedge_pos < 0.0;
            let hl_side = if hl_is_buy {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            };
            let hl_size = hedge_pos.abs();
            let hl_price_hint = if hl_is_buy { hl_ask } else { hl_bid };

            self.hyperliquid_trading
                .place_market_order(
                    hedge_symbol,
                    hl_is_buy,
                    hl_size,
                    self.config.hyperliquid_slippage,
                    true, // reduce_only for flatten
                    Some(hl_bid),
                    Some(hl_ask),
                )
                .await
                .with_context(|| {
                    format!(
                        "Failed Hyperliquid exit market order ({} {} {})",
                        hedge_symbol,
                        if hl_is_buy { "BUY" } else { "SELL" },
                        hl_size
                    )
                })?;

            let order_csv = audit_logger::order_file_for_symbol(hedge_symbol);
            let order_record = audit_logger::OrderRecord::new(
                chrono::Utc::now(),
                hedge_symbol.to_string(),
                "Hyperliquid".to_string(),
                hl_side,
                hl_size,
                hl_price_hint.max(0.0),
                None,
                None,
                "exit_rebalance_market_flatten".to_string(),
            );
            if let Err(e) = audit_logger::log_order(&order_csv, &order_record) {
                info!("[EXIT] {} Failed to log hedge exit order: {}", "⚠".yellow().bold(), e);
            }
        }

        Ok(true)
    }

    /// Run the bot - spawn all services and execute main loop
    pub async fn run(mut self) -> Result<()> {
        let maker_symbol = self.config.maker_symbol_str().to_string();
        let hedge_symbol = self.config.hedge_symbol_str().to_string();
        let symbol_pair = format!("{}|{}", maker_symbol, hedge_symbol);

        // ═══════════════════════════════════════════════════
        // SPAWN ALL SERVICES
        // ═══════════════════════════════════════════════════

        // Service 1: Maker orderbook WebSocket (Pacifica only for now)
        if matches!(self.maker_kind, MakerExchangeKind::Pacifica) {
            let pacifica_ob_service = PacificaOrderbookService {
                prices: self.pacifica_prices.clone(),
                symbol: maker_symbol.clone(),
                agg_level: self.config.agg_level,
                reconnect_attempts: self.config.reconnect_attempts,
                ping_interval_secs: self.config.ping_interval_secs,
            };
            tokio::spawn(async move {
                pacifica_ob_service.run().await.ok();
            });
        } else {
            info!(
                "{} Using REST polling for maker orderbook ({})",
                "[INIT]".cyan().bold(),
                self.maker_exchange.name()
            );
        }

        // Service 2: Hyperliquid Orderbook (WebSocket)
        let hyperliquid_ob_service = HyperliquidOrderbookService {
            prices: self.hyperliquid_prices.clone(),
            symbol: hedge_symbol.clone(),
            reconnect_attempts: self.config.reconnect_attempts,
            ping_interval_secs: self.config.ping_interval_secs,
        };
        tokio::spawn(async move {
            hyperliquid_ob_service.run().await.ok();
        });
        // Service 3: Pacifica WebSocket fill detection (only when maker = Pacifica)
        if let (MakerExchangeKind::Pacifica, Some(pac_creds), Some(pac_trading_fill), Some(pac_ws)) = (
            self.maker_kind,
            self.pacifica_credentials.clone(),
            self.pacifica_trading_fill.clone(),
            self.pacifica_ws_trading.clone(),
        ) {
            let fill_config = FillDetectionConfig {
                account: pac_creds.account,
                reconnect_attempts: self.config.reconnect_attempts,
                ping_interval_secs: self.config.ping_interval_secs,
                enable_position_fill_detection: true,
            };
            let fill_client = FillDetectionClient::new(fill_config.clone(), false)
                .context("Failed to create fill detection client")?;
            let baseline_updater = fill_client.get_baseline_updater();

            let fill_service = FillDetectionService {
                bot_state: self.bot_state.clone(),
                hedge_tx: self.hedge_tx.clone(),
                pacifica_trading: pac_trading_fill,
                pacifica_ws_trading: pac_ws,
                fill_config,
                symbol: maker_symbol.clone(),
                processed_fills: self.processed_fills.clone(),
                baseline_updater,
                atomic_status: self.atomic_status.clone(),
                order_snapshot: self.order_snapshot.clone(),
            };
            tokio::spawn(async move {
                fill_service.run().await;
            });
        }
        if matches!(self.maker_kind, MakerExchangeKind::Binance) {
            let binance_fill_service = BinanceFillDetectionService {
                bot_state: self.bot_state.clone(),
                hedge_tx: self.hedge_tx.clone(),
                maker_exchange: self.maker_exchange.clone(),
                symbol: maker_symbol.clone(),
                processed_fills: self.processed_fills.clone(),
                reconnect_attempts: self.config.reconnect_attempts,
                ping_interval_secs: self.config.ping_interval_secs,
                min_hedge_notional: 10.0,
            };
            tokio::spawn(async move {
                binance_fill_service.run().await;
            });
        }

        // Service 4: Maker REST Poll (price redundancy)
        let maker_rest_poll_service = MakerRestPollService {
            prices: self.pacifica_prices.clone(),
            maker_exchange: self.maker_exchange.clone(),
            symbol: maker_symbol.clone(),
            agg_level: self.config.agg_level,
            poll_interval_secs: self.config.pacifica_rest_poll_interval_secs,
        };
        tokio::spawn(async move {
            maker_rest_poll_service.run().await;
        });

        // Service 4.5: Hyperliquid REST Poll (price redundancy)
        let hyperliquid_rest_poll_service = HyperliquidRestPollService {
            prices: self.hyperliquid_prices.clone(),
            hyperliquid_trading: self.hyperliquid_trading.clone(),
            symbol: hedge_symbol.clone(),
            poll_interval_secs: 2,
        };
        tokio::spawn(async move {
            hyperliquid_rest_poll_service.run().await;
        });

        // Service 4.6: Spread recorder (1-second sampling, 24h retention)
        let spread_recorder_service = SpreadRecorderService {
            symbol: symbol_pair.clone(),
            maker_exchange: self.maker_exchange.name().to_string(),
            maker_prices: self.pacifica_prices.clone(),
            hyperliquid_prices: self.hyperliquid_prices.clone(),
            file_path: format!(
                "{}_{}_spread_history.csv",
                maker_symbol.to_lowercase(),
                hedge_symbol.to_lowercase().replace(':', "_")
            ),
            sample_interval_secs: 1,
            retention_hours: 24,
        };
        tokio::spawn(async move {
            spread_recorder_service.run().await;
        });

        // Wait for initial orderbook data
        info!("{} Waiting for orderbook data...", "[INIT]".cyan().bold());
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Service 5: REST Fill Detection (backup)
        let rest_fill_service = RestFillDetectionService {
            bot_state: self.bot_state.clone(),
            hedge_tx: self.hedge_tx.clone(),
            maker_exchange: self.maker_exchange.clone(),
            symbol: maker_symbol.clone(),
            processed_fills: self.processed_fills.clone(),
            min_hedge_notional: 10.0,
            poll_interval_ms: self.config.pacifica_active_order_rest_poll_interval_ms,
        };
        tokio::spawn(async move {
            rest_fill_service.run().await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Service 5.5: Pacifica position monitor (ground truth, Pacifica only)
        if let (MakerExchangeKind::Pacifica, Some(pac_trading), Some(pac_ws)) = (
            self.maker_kind,
            self.pacifica_trading_fill.clone(),
            self.pacifica_ws_trading.clone(),
        ) {
            let position_monitor_service = PositionMonitorService {
                bot_state: self.bot_state.clone(),
                hedge_tx: self.hedge_tx.clone(),
                pacifica_trading: pac_trading,
                pacifica_ws_trading: pac_ws,
                symbol: maker_symbol.clone(),
                processed_fills: self.processed_fills.clone(),
                last_position_snapshot: self.last_position_snapshot.clone(),
            };
            tokio::spawn(async move {
                position_monitor_service.run().await;
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Service 6: Order Monitor (age/profit monitoring)
        let (order_monitor_service, cancel_rx) = OrderMonitorService::new(
            self.bot_state.clone(),
            self.atomic_status.clone(),
            self.order_snapshot.clone(),
            self.pacifica_prices.clone(),
            self.hyperliquid_prices.clone(),
            self.config.clone(),
            maker_symbol.clone(),
            hedge_symbol.clone(),
            self.normal_evaluator.clone(),
            self.maker_exchange.clone(),
            self.hyperliquid_trading.clone(),
        );
        let order_monitor_service = Arc::new(order_monitor_service);
        spawn_monitor_tasks(order_monitor_service, cancel_rx);

        // Service 7: Hedge Execution
        let hedge_service = HedgeService {
            bot_state: self.bot_state.clone(),
            hedge_rx: self.hedge_rx.take().unwrap(),
            hyperliquid_prices: self.hyperliquid_prices.clone(),
            config: self.config.clone(),
            maker_symbol: maker_symbol.clone(),
            hedge_symbol: hedge_symbol.clone(),
            hyperliquid_trading: self.hyperliquid_trading.clone(),
            maker_exchange: self.maker_exchange.clone(),
            shutdown_tx: self.shutdown_tx.clone(),
            hl_wallet: self.hl_wallet.clone(),
        };
        tokio::spawn(async move {
            hedge_service.run().await;
        });

        // ═══════════════════════════════════════════════════
        // MAIN OPPORTUNITY EVALUATION LOOP
        // ═══════════════════════════════════════════════════

        info!("{} Starting opportunity evaluation loop",
            format!("[{} MAIN]", symbol_pair).bright_white().bold()
        );
        info!("");

        let mut eval_interval = interval(Duration::from_millis(
            self.config.evaluation_loop_interval_ms.max(1),
        ));
        let mut order_placement_rate_limit = RateLimitTracker::new();
        let mut next_order_attempt_allowed_at = Instant::now();
        let mut exit_rebalance_below_threshold_since: Option<Instant> = None;
        let mut next_exit_rebalance_allowed_at = Instant::now();
        let mut last_event_active = false;
        let mut regime = RegimeController::new(RegimeSettings {
            strategy_mode: self.config.strategy_mode,
            event_trigger_mid_spread_bps: self.config.event_trigger_mid_spread_bps,
            event_trigger_hl_spread_bps: self.config.event_trigger_hl_spread_bps,
            event_trigger_confirm_secs: self.config.event_trigger_confirm_secs,
        });

        let sigint = signal::ctrl_c();
        tokio::pin!(sigint);
        
        #[cfg(unix)]
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to setup SIGTERM handler");

        let mut shutdown_rx = self.shutdown_rx.take().unwrap();

        loop {
            tokio::select! {
                _ = &mut sigint => {
                    info!("{} {} Received SIGINT (Ctrl+C), initiating graceful shutdown...",
                        format!("[{} MAIN]", symbol_pair).bright_white().bold(),
                        "⚠".yellow().bold()
                    );
                    break;
                }

                _ = async {
                    #[cfg(unix)]
                    {
                        sigterm.recv().await
                    }
                    #[cfg(not(unix))]
                    {
                        std::future::pending::<Option<()>>().await
                    }
                } => {
                    info!("{} {} Received SIGTERM (Docker shutdown), initiating graceful shutdown...",
                        format!("[{} MAIN]", symbol_pair).bright_white().bold(),
                        "⚠".yellow().bold()
                    );
                    break;
                }

                _ = eval_interval.tick() => {
                    // Check if we should exit
                    let state = self.bot_state.read().await;
                    if state.is_terminal() {
                        break;
                    }

                    // Only evaluate if idle
                    if !state.is_idle() {
                        continue;
                    }

                    // Check grace period
                    if !state.grace_period_elapsed(3) {
                        continue;
                    }
                    drop(state);

                    // Check rate limit backoff
                    if order_placement_rate_limit.should_skip() {
                        let remaining = order_placement_rate_limit.remaining_backoff_secs();
                        if remaining as u64 % 5 == 0 || remaining < 1.0 {
                            debug!("[MAIN] Skipping order placement (rate limit backoff, {:.1}s remaining)", remaining);
                        }
                        continue;
                    }
                    if Instant::now() < next_order_attempt_allowed_at {
                        continue;
                    }

                    // Get current prices
                    let (pac_bid, pac_ask) = *self.pacifica_prices.lock();
                    let (hl_bid, hl_ask) = *self.hyperliquid_prices.lock();

                    // Validate prices
                    if pac_bid == 0.0 || pac_ask == 0.0 || hl_bid == 0.0 || hl_ask == 0.0 {
                        continue;
                    }

                    let pac_mid = (pac_bid + pac_ask) / 2.0;
                    let hl_mid = (hl_bid + hl_ask) / 2.0;
                    if hl_mid <= 0.0 {
                        continue;
                    }
                    let mid_spread_bps = ((pac_mid - hl_mid) / hl_mid) * 10000.0;
                    let abs_mid_spread_bps = mid_spread_bps.abs();
                    let hl_spread_bps = ((hl_ask - hl_bid) / hl_mid) * 10000.0;
                    let regime_decision =
                        regime.update(Instant::now(), mid_spread_bps, hl_spread_bps.max(0.0));
                    if regime_decision.event_active && !last_event_active {
                        info!(
                            "[REGIME] {} Event mode activated | mid={:+.2}bps hl_spread={:.2}bps direction={}",
                            "⚡".yellow().bold(),
                            mid_spread_bps,
                            hl_spread_bps.max(0.0),
                            regime_decision
                                .direction_filter
                                .map(|s| s.as_str())
                                .unwrap_or("-")
                        );
                    } else if !regime_decision.event_active && last_event_active {
                        info!(
                            "[REGIME] {} Back to normal mode",
                            "✓".green().bold()
                        );
                    }
                    last_event_active = regime_decision.event_active;

                    if self.config.enable_exit_rebalance && regime_decision.event_active {
                        if abs_mid_spread_bps <= self.config.exit_rebalance_exit_spread_bps {
                            if exit_rebalance_below_threshold_since.is_none() {
                                exit_rebalance_below_threshold_since = Some(Instant::now());
                                info!(
                                    "[EXIT] Spread back to {:.2} bps (<= {:.2}), waiting {}s confirmation",
                                    abs_mid_spread_bps,
                                    self.config.exit_rebalance_exit_spread_bps,
                                    self.config.exit_rebalance_confirm_secs
                                );
                            }

                            if let Some(since) = exit_rebalance_below_threshold_since {
                                if since.elapsed()
                                    >= Duration::from_secs(self.config.exit_rebalance_confirm_secs)
                                {
                                    if Instant::now() >= next_exit_rebalance_allowed_at {
                                        match self
                                            .trigger_exit_rebalance(
                                                &maker_symbol,
                                                &hedge_symbol,
                                                pac_bid,
                                                pac_ask,
                                                hl_bid,
                                                hl_ask,
                                                abs_mid_spread_bps,
                                            )
                                            .await
                                        {
                                            Ok(triggered) => {
                                                if triggered {
                                                    info!(
                                                        "[EXIT] {} Event spread flattened and rebalanced",
                                                        "✓".green().bold()
                                                    );
                                                } else {
                                                    info!("[EXIT] Event ended without residual position");
                                                }
                                                regime.clear_event_with_cooldown(
                                                    self.config.event_rearm_cooldown_secs,
                                                );
                                                exit_rebalance_below_threshold_since = None;
                                                next_exit_rebalance_allowed_at = Instant::now()
                                                    + Duration::from_secs(
                                                        self.config
                                                            .exit_rebalance_cooldown_secs
                                                            .max(1),
                                                    );
                                                next_order_attempt_allowed_at =
                                                    Instant::now() + Duration::from_secs(2);
                                            }
                                            Err(e) => {
                                                info!(
                                                    "[EXIT] {} Exit/rebalance failed: {}",
                                                    "✗".red().bold(),
                                                    e
                                                );
                                                exit_rebalance_below_threshold_since = None;
                                                next_exit_rebalance_allowed_at = Instant::now()
                                                    + Duration::from_secs(
                                                        self.config
                                                            .exit_rebalance_cooldown_secs
                                                            .max(1),
                                                    );
                                            }
                                        }
                                    }
                                    // While event is reverting, suppress new entries.
                                    continue;
                                }
                            }
                            continue;
                        } else {
                            exit_rebalance_below_threshold_since = None;
                        }
                    } else {
                        exit_rebalance_below_threshold_since = None;
                    }

                    if !self.config.enable_exit_rebalance && regime_decision.event_active {
                        if abs_mid_spread_bps <= self.config.exit_rebalance_exit_spread_bps {
                            if exit_rebalance_below_threshold_since.is_none() {
                                exit_rebalance_below_threshold_since = Some(Instant::now());
                            }
                            if let Some(since) = exit_rebalance_below_threshold_since {
                                if since.elapsed()
                                    >= Duration::from_secs(self.config.exit_rebalance_confirm_secs)
                                {
                                    regime.clear_event_with_cooldown(
                                        self.config.event_rearm_cooldown_secs,
                                    );
                                    exit_rebalance_below_threshold_since = None;
                                    info!(
                                        "[REGIME] Event mode released by spread normalization (flatten disabled)"
                                    );
                                }
                            }
                        } else {
                            exit_rebalance_below_threshold_since = None;
                        }
                    }

                    if regime_decision.suppress_entries {
                        continue;
                    }

                    // Get timestamp once for this evaluation cycle
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;

                    let (active_profile, active_evaluator, profile_label) = match regime_decision.profile
                    {
                        EntryProfile::Normal => (&self.normal_mode, &self.normal_evaluator, "normal"),
                        EntryProfile::Event => (&self.event_mode, &self.event_evaluator, "event"),
                    };

                    // Evaluate opportunities
                    let buy_opp = active_evaluator
                        .evaluate_buy_opportunity(hl_bid, active_profile.order_notional_usd, now_ms);
                    let sell_opp = active_evaluator
                        .evaluate_sell_opportunity(hl_ask, active_profile.order_notional_usd, now_ms);

                    let buy_opp = match regime_decision.direction_filter {
                        Some(OrderSide::Buy) | None => buy_opp,
                        Some(OrderSide::Sell) => None,
                    };
                    let sell_opp = match regime_decision.direction_filter {
                        Some(OrderSide::Sell) | None => sell_opp,
                        Some(OrderSide::Buy) => None,
                    };

                    let best_opp = OpportunityEvaluator::pick_best_opportunity(buy_opp, sell_opp, pac_mid);

                    if let Some(opp) = best_opp {
                        // Double-check bot is still idle
                        let mut state = self.bot_state.write().await;
                        if !state.is_idle() {
                            continue;
                        }

                        // Keep maker orders strictly post-only:
                        // - BUY price must not cross ask
                        // - SELL price must not cross bid
                        // Using current maker TOB gives a conservative, exchange-safe price.
                        let maker_limit_price = match opp.direction {
                            OrderSide::Buy => opp.pacifica_price.min(pac_bid),
                            OrderSide::Sell => opp.pacifica_price.max(pac_ask),
                        };
                        let initial_profit_bps = active_evaluator.recalculate_profit_raw(
                            opp.direction,
                            maker_limit_price,
                            hl_bid,
                            hl_ask,
                        );

                        info!(
                            "{} [{}] {} @ {} (raw:{} ) → HL {} | Size: {} | Profit: {} | PAC: {}/{} | HL: {}/{}",
                            format!("[{} OPPORTUNITY]", symbol_pair).bright_green().bold(),
                            profile_label.to_uppercase().bright_white(),
                            opp.direction.as_str().bright_yellow().bold(),
                            format!("${:.6}", maker_limit_price).cyan().bold(),
                            format!("${:.6}", opp.pacifica_price).bright_black(),
                            format!("${:.6}", opp.hyperliquid_price).cyan(),
                            format!("{:.4}", opp.size).bright_white(),
                            format!("{:.2} bps", initial_profit_bps).green().bold(),
                            format!("${:.6}", pac_bid).cyan(),
                            format!("${:.6}", pac_ask).cyan(),
                            format!("${:.6}", hl_bid).cyan(),
                            format!("${:.6}", hl_ask).cyan()
                        );

                        // Place order
                        info!("{} Placing {} on {}...",
                            format!("[{} ORDER]", symbol_pair).bright_yellow().bold(),
                            opp.direction.as_str().bright_yellow().bold(),
                            self.maker_exchange.name()
                        );

                        let maker_side = MakerOrderSide::from_strategy_side(opp.direction);

                        match self.maker_exchange
                            .place_limit_order(
                                &maker_symbol,
                                maker_side,
                                opp.size,
                                maker_limit_price,
                                Some(pac_bid),
                                Some(pac_ask),
                            )
                            .await
                        {
                            Ok(order_data) => {
                                order_placement_rate_limit.record_success();

                                let client_order_id = order_data
                                    .client_order_id
                                    .clone()
                                    .or_else(|| order_data.order_id.clone());

                                if let Some(client_order_id) = client_order_id {
                                    let order_id = order_data.order_id.clone().unwrap_or_else(|| "-".to_string());
                                    let cloid_head = client_order_id.chars().take(8).collect::<String>();
                                    let cloid_tail = {
                                        let chars: Vec<char> = client_order_id.chars().collect();
                                        let n = chars.len();
                                        let start = n.saturating_sub(4);
                                        chars[start..n].iter().collect::<String>()
                                    };
                                    info!(
                                        "{} {} Placed {} #{} @ {} | cloid: {}...{}",
                                        format!("[{} ORDER]", symbol_pair).bright_yellow().bold(),
                                        "✓".green().bold(),
                                        opp.direction.as_str().bright_yellow(),
                                        order_id,
                                        format!("${:.4}", maker_limit_price).cyan().bold(),
                                        cloid_head,
                                        cloid_tail
                                    );

                                    let order_csv = audit_logger::order_file_for_symbol(&maker_symbol);
                                    let order_record = audit_logger::OrderRecord::new(
                                        chrono::Utc::now(),
                                        maker_symbol.clone(),
                                        self.maker_exchange.name().to_string(),
                                        opp.direction,
                                        opp.size,
                                        maker_limit_price,
                                        order_data.order_id.clone(),
                                        Some(client_order_id.clone()),
                                        format!("main_loop_limit_order_{}", profile_label),
                                    );
                                    if let Err(e) = audit_logger::log_order(&order_csv, &order_record) {
                                        info!(
                                            "{} {} Failed to log order to CSV: {}",
                                            format!("[{} ORDER]", symbol_pair).bright_yellow().bold(),
                                            "⚠".yellow().bold(),
                                            e
                                        );
                                    }

                                    let placed_at = Instant::now();
                                    let active_order = ActiveOrder {
                                        client_order_id: client_order_id.clone(),
                                        exchange_order_id: order_data.order_id.clone(),
                                        symbol: maker_symbol.clone(),
                                        side: opp.direction,
                                        price: maker_limit_price,
                                        size: opp.size,
                                        initial_profit_bps,
                                        placed_at,
                                    };

                                    state.set_active_order(active_order);

                                    // Sync atomic status and order snapshot for order monitor
                                    sync_atomic_status(&self.atomic_status, &state.status);
                                    update_order_snapshot(
                                        &self.order_snapshot,
                                        client_order_id.clone(),
                                        opp.direction,
                                        maker_limit_price,
                                        opp.size,
                                        initial_profit_bps,
                                        placed_at,
                                        active_profile.profit_cancel_threshold_bps,
                                        active_profile.order_refresh_interval_secs,
                                    );
                                } else {
                                    if self.config.order_failure_cooldown_ms > 0 {
                                        next_order_attempt_allowed_at = Instant::now()
                                            + Duration::from_millis(self.config.order_failure_cooldown_ms);
                                    }
                                    info!("{} {} Order placed but no id returned",
                                        format!("[{} ORDER]", symbol_pair).bright_yellow().bold(),
                                        "✗".red().bold()
                                    );
                                }
                            }
                            Err(e) => {
                                if is_rate_limit_error(&e) {
                                    order_placement_rate_limit.record_error();
                                    let backoff_secs = order_placement_rate_limit.get_backoff_secs();
                                    let cooldown_target = Instant::now()
                                        + Duration::from_secs(backoff_secs.max(1));
                                    if cooldown_target > next_order_attempt_allowed_at {
                                        next_order_attempt_allowed_at = cooldown_target;
                                    }
                                    info!(
                                        "{} {} Failed to place order: Rate limit exceeded. Backing off for {}s (attempt #{})",
                                        format!("[{} ORDER]", symbol_pair).bright_yellow().bold(),
                                        "⚠".yellow().bold(),
                                        backoff_secs,
                                        order_placement_rate_limit.consecutive_errors()
                                    );
                                } else {
                                    let is_post_only_reject = is_post_only_reject_error(&e);
                                    let cooldown_ms = if is_post_only_reject {
                                        self.config.post_only_reject_cooldown_ms
                                    } else {
                                        self.config.order_failure_cooldown_ms
                                    };
                                    if cooldown_ms > 0 {
                                        next_order_attempt_allowed_at = Instant::now()
                                            + Duration::from_millis(cooldown_ms);
                                    }
                                    info!("{} {} Failed to place order: {}",
                                        format!("[{} ORDER]", symbol_pair).bright_yellow().bold(),
                                        "✗".red().bold(),
                                        e.to_string().red()
                                    );
                                    info!(
                                        "{} Cooldown after failure: {} ms ({})",
                                        format!("[{} ORDER]", symbol_pair).bright_yellow().bold(),
                                        cooldown_ms,
                                        if is_post_only_reject { "post_only_reject" } else { "generic_failure" }
                                    );
                                }
                            }
                        }
                    }
                }

                _ = shutdown_rx.recv() => {
                    info!("{} Shutdown signal received",
                        format!("[{} MAIN]", symbol_pair).bright_white().bold()
                    );
                    break;
                }
            }
        }

        // ═══════════════════════════════════════════════════
        // SHUTDOWN CLEANUP
        // ═══════════════════════════════════════════════════

        info!("");
        info!("{} Cancelling any remaining orders...",
            format!("[{} SHUTDOWN]", symbol_pair).yellow().bold()
        );

        match self.maker_exchange.cancel_all_orders(Some(&maker_symbol)).await {
            Ok(count) => info!("{} {} Cancelled {} order(s)",
                format!("[{} SHUTDOWN]", symbol_pair).yellow().bold(),
                "✓".green().bold(),
                count
            ),
            Err(e) => info!("{} {} Failed to cancel orders: {}",
                format!("[{} SHUTDOWN]", symbol_pair).yellow().bold(),
                "⚠".yellow().bold(),
                e
            ),
        }

        // Final state check
        let final_state = self.bot_state.read().await;
        match &final_state.status {
            BotStatus::Complete | BotStatus::Idle | BotStatus::OrderPlaced | BotStatus::Filled | BotStatus::Hedging => {
                info!("");
                info!("{} {}", "✓".green().bold(), "Bot terminated gracefully.".green().bold());
                info!("Final position: {}", final_state.position);
                Ok(())
            }
            BotStatus::Error(e) => {
                info!("");
                info!("{} {}: {}", "✗".red().bold(), "Bot terminated with error".red().bold(), e.to_string().red());
                anyhow::bail!("Bot failed: {}", e)
            }
        }
    }
}
