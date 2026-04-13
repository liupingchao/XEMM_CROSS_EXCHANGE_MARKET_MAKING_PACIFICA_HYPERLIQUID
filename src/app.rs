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

use crate::bot::{ActiveOrder, BotState, BotStatus};
use crate::config::Config;
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
use crate::strategy::OpportunityEvaluator;
use crate::util::rate_limit::{is_rate_limit_error, RateLimitTracker};



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
    pub evaluator: OpportunityEvaluator,

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

        info!("{} Symbol: {}",
            "[CONFIG]".blue().bold(),
            config.symbol.bright_white().bold()
        );
        info!("{} Order Notional: {}",
            "[CONFIG]".blue().bold(),
            format!("${:.2}", config.order_notional_usd).bright_white()
        );
        info!("{} Pacifica Maker Fee: {}",
            "[CONFIG]".blue().bold(),
            format!("{} bps", config.pacifica_maker_fee_bps).bright_white()
        );
        info!("{} Hyperliquid Taker Fee: {}",
            "[CONFIG]".blue().bold(),
            format!("{} bps", config.hyperliquid_taker_fee_bps).bright_white()
        );
        info!("{} Target Profit: {}",
            "[CONFIG]".blue().bold(),
            format!("{} bps", config.profit_rate_bps).green().bold()
        );
        info!("{} Profit Cancel Threshold: {}",
            "[CONFIG]".blue().bold(),
            format!("{} bps", config.profit_cancel_threshold_bps).yellow()
        );
        info!("{} Order Refresh Interval: {}",
            "[CONFIG]".blue().bold(),
            format!("{} secs", config.order_refresh_interval_secs).bright_white()
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
            config.symbol.bright_white()
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
        match maker_exchange.cancel_all_orders(Some(&config.symbol)).await {
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
            .get_symbol_info(&config.symbol)
            .await
            .with_context(|| format!("Failed to fetch {} symbol info", maker_exchange.name()))?;
        let maker_tick_size = maker_symbol_info.tick_size;

        info!("{} {} tick size for {}: {}",
            "[INIT]".cyan().bold(),
            maker_exchange.name(),
            config.symbol.bright_white(),
            format!("{}", maker_tick_size).bright_white()
        );

        // Create opportunity evaluator
        let evaluator = OpportunityEvaluator::new(
            config.pacifica_maker_fee_bps,
            config.hyperliquid_taker_fee_bps,
            config.profit_rate_bps,
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
            hyperliquid_trading,
            pacifica_prices,
            hyperliquid_prices,
            evaluator,
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

    /// Run the bot - spawn all services and execute main loop
    pub async fn run(mut self) -> Result<()> {
        // ═══════════════════════════════════════════════════
        // SPAWN ALL SERVICES
        // ═══════════════════════════════════════════════════

        // Service 1: Maker orderbook WebSocket (Pacifica only for now)
        if matches!(self.maker_kind, MakerExchangeKind::Pacifica) {
            let pacifica_ob_service = PacificaOrderbookService {
                prices: self.pacifica_prices.clone(),
                symbol: self.config.symbol.clone(),
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
            symbol: self.config.symbol.clone(),
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
                symbol: self.config.symbol.clone(),
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
                symbol: self.config.symbol.clone(),
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
            symbol: self.config.symbol.clone(),
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
            symbol: self.config.symbol.clone(),
            poll_interval_secs: 2,
        };
        tokio::spawn(async move {
            hyperliquid_rest_poll_service.run().await;
        });

        // Service 4.6: Spread recorder (1-second sampling, 24h retention)
        let spread_recorder_service = SpreadRecorderService {
            symbol: self.config.symbol.clone(),
            maker_exchange: self.maker_exchange.name().to_string(),
            maker_prices: self.pacifica_prices.clone(),
            hyperliquid_prices: self.hyperliquid_prices.clone(),
            file_path: format!("{}_spread_history.csv", self.config.symbol.to_lowercase()),
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
            symbol: self.config.symbol.clone(),
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
                symbol: self.config.symbol.clone(),
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
            self.evaluator.clone(),
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
            hyperliquid_trading: self.hyperliquid_trading.clone(),
            maker_exchange: self.maker_exchange.clone(),
            shutdown_tx: self.shutdown_tx.clone(),
        };
        tokio::spawn(async move {
            hedge_service.run().await;
        });

        // ═══════════════════════════════════════════════════
        // MAIN OPPORTUNITY EVALUATION LOOP
        // ═══════════════════════════════════════════════════

        info!("{} Starting opportunity evaluation loop",
            format!("[{} MAIN]", self.config.symbol).bright_white().bold()
        );
        info!("");

        let mut eval_interval = interval(Duration::from_millis(1));
        let mut order_placement_rate_limit = RateLimitTracker::new();

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
                        format!("[{} MAIN]", self.config.symbol).bright_white().bold(),
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
                        format!("[{} MAIN]", self.config.symbol).bright_white().bold(),
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

                    // Get current prices
                    let (pac_bid, pac_ask) = *self.pacifica_prices.lock();
                    let (hl_bid, hl_ask) = *self.hyperliquid_prices.lock();

                    // Validate prices
                    if pac_bid == 0.0 || pac_ask == 0.0 || hl_bid == 0.0 || hl_ask == 0.0 {
                        continue;
                    }

                    // Get timestamp once for this evaluation cycle
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;

                    // Evaluate opportunities
                    let buy_opp = self.evaluator.evaluate_buy_opportunity(hl_bid, self.config.order_notional_usd, now_ms);
                    let sell_opp = self.evaluator.evaluate_sell_opportunity(hl_ask, self.config.order_notional_usd, now_ms);

                    let pac_mid = (pac_bid + pac_ask) / 2.0;
                    let best_opp = OpportunityEvaluator::pick_best_opportunity(buy_opp, sell_opp, pac_mid);

                    if let Some(opp) = best_opp {
                        // Double-check bot is still idle
                        let mut state = self.bot_state.write().await;
                        if !state.is_idle() {
                            continue;
                        }

                        info!(
                            "{} {} @ {} → HL {} | Size: {} | Profit: {} | PAC: {}/{} | HL: {}/{}",
                            format!("[{} OPPORTUNITY]", self.config.symbol).bright_green().bold(),
                            opp.direction.as_str().bright_yellow().bold(),
                            format!("${:.6}", opp.pacifica_price).cyan().bold(),
                            format!("${:.6}", opp.hyperliquid_price).cyan(),
                            format!("{:.4}", opp.size).bright_white(),
                            format!("{:.2} bps", opp.initial_profit_bps).green().bold(),
                            format!("${:.6}", pac_bid).cyan(),
                            format!("${:.6}", pac_ask).cyan(),
                            format!("${:.6}", hl_bid).cyan(),
                            format!("${:.6}", hl_ask).cyan()
                        );

                        // Place order
                        info!("{} Placing {} on {}...",
                            format!("[{} ORDER]", self.config.symbol).bright_yellow().bold(),
                            opp.direction.as_str().bright_yellow().bold(),
                            self.maker_exchange.name()
                        );

                        let maker_side = MakerOrderSide::from_strategy_side(opp.direction);

                        match self.maker_exchange
                            .place_limit_order(
                                &self.config.symbol,
                                maker_side,
                                opp.size,
                                opp.pacifica_price,
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
                                        format!("[{} ORDER]", self.config.symbol).bright_yellow().bold(),
                                        "✓".green().bold(),
                                        opp.direction.as_str().bright_yellow(),
                                        order_id,
                                        format!("${:.4}", opp.pacifica_price).cyan().bold(),
                                        cloid_head,
                                        cloid_tail
                                    );

                                    let active_order = ActiveOrder {
                                        client_order_id,
                                        exchange_order_id: order_data.order_id.clone(),
                                        symbol: self.config.symbol.clone(),
                                        side: opp.direction,
                                        price: opp.pacifica_price,
                                        size: opp.size,
                                        initial_profit_bps: opp.initial_profit_bps,
                                        placed_at: Instant::now(),
                                    };

                                    state.set_active_order(active_order);

                                    // Sync atomic status and order snapshot for order monitor
                                    sync_atomic_status(&self.atomic_status, &state.status);
                                    update_order_snapshot(
                                        &self.order_snapshot,
                                        opp.direction,
                                        opp.pacifica_price,
                                        opp.size,
                                        opp.initial_profit_bps,
                                    );
                                } else {
                                    info!("{} {} Order placed but no id returned",
                                        format!("[{} ORDER]", self.config.symbol).bright_yellow().bold(),
                                        "✗".red().bold()
                                    );
                                }
                            }
                            Err(e) => {
                                if is_rate_limit_error(&e) {
                                    order_placement_rate_limit.record_error();
                                    let backoff_secs = order_placement_rate_limit.get_backoff_secs();
                                    info!(
                                        "{} {} Failed to place order: Rate limit exceeded. Backing off for {}s (attempt #{})",
                                        format!("[{} ORDER]", self.config.symbol).bright_yellow().bold(),
                                        "⚠".yellow().bold(),
                                        backoff_secs,
                                        order_placement_rate_limit.consecutive_errors()
                                    );
                                } else {
                                    info!("{} {} Failed to place order: {}",
                                        format!("[{} ORDER]", self.config.symbol).bright_yellow().bold(),
                                        "✗".red().bold(),
                                        e.to_string().red()
                                    );
                                }
                            }
                        }
                    }
                }

                _ = shutdown_rx.recv() => {
                    info!("{} Shutdown signal received",
                        format!("[{} MAIN]", self.config.symbol).bright_white().bold()
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
            format!("[{} SHUTDOWN]", self.config.symbol).yellow().bold()
        );

        match self.maker_exchange.cancel_all_orders(Some(&self.config.symbol)).await {
            Ok(count) => info!("{} {} Cancelled {} order(s)",
                format!("[{} SHUTDOWN]", self.config.symbol).yellow().bold(),
                "✓".green().bold(),
                count
            ),
            Err(e) => info!("{} {} Failed to cancel orders: {}",
                format!("[{} SHUTDOWN]", self.config.symbol).yellow().bold(),
                "⚠".yellow().bold(),
                e
            ),
        }

        // Final state check
        let final_state = self.bot_state.read().await;
        match &final_state.status {
            BotStatus::Complete => {
                info!("");
                info!("{} {}", "✓".green().bold(), "Bot completed successfully!".green().bold());
                info!("Final position: {}", final_state.position);
                Ok(())
            }
            BotStatus::Error(e) => {
                info!("");
                info!("{} {}: {}", "✗".red().bold(), "Bot terminated with error".red().bold(), e.to_string().red());
                anyhow::bail!("Bot failed: {}", e)
            }
            _ => {
                info!("");
                info!("{} Bot terminated in unexpected state: {:?}", "⚠".yellow().bold(), final_state.status);
                Ok(())
            }
        }
    }
}
