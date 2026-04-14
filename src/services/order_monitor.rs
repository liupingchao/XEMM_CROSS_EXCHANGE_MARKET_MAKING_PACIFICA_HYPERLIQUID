use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::bot::{BotState, BotStatus};
use crate::config::Config;
use crate::connector::hyperliquid::HyperliquidTrading;
use crate::connector::maker::MakerExchange;
use crate::strategy::{OpportunityEvaluator, OrderSide};
use crate::util::rate_limit::{is_rate_limit_error, RateLimitTracker};

// ============================================================================
// ATOMIC STATUS FOR LOCK-FREE HOT PATH CHECKS
// ============================================================================

/// Atomic bot status for lock-free checks in hot path
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicBotStatus {
    Idle = 0,
    OrderPlaced = 1,
    Filled = 2,
    Hedging = 3,
    Complete = 4,
    Error = 5,
}

impl From<&BotStatus> for AtomicBotStatus {
    fn from(status: &BotStatus) -> Self {
        match status {
            BotStatus::Idle => AtomicBotStatus::Idle,
            BotStatus::OrderPlaced => AtomicBotStatus::OrderPlaced,
            BotStatus::Filled => AtomicBotStatus::Filled,
            BotStatus::Hedging => AtomicBotStatus::Hedging,
            BotStatus::Complete => AtomicBotStatus::Complete,
            BotStatus::Error(_) => AtomicBotStatus::Error,
        }
    }
}

// ============================================================================
// LIGHTWEIGHT ORDER SNAPSHOT (AVOID CLONING FULL ORDER)
// ============================================================================

/// Minimal order data needed for monitoring (avoids full clone)
#[derive(Debug, Clone)]
pub struct OrderSnapshot {
    pub client_order_id: String,
    pub side: OrderSide,
    pub price: f64,
    pub size: f64,
    pub initial_profit_bps: f64,
    pub placed_at: Instant,
    pub profit_cancel_threshold_bps: f64,
    pub order_refresh_interval_secs: u64,
}

/// Shared order snapshot updated atomically by order placer
/// Uses Option wrapped in Mutex for atomic swap semantics
pub struct SharedOrderSnapshot {
    inner: Mutex<Option<OrderSnapshot>>,
}

impl SharedOrderSnapshot {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    #[inline]
    pub fn get(&self) -> Option<OrderSnapshot> {
        self.inner.lock().clone()
    }

    #[inline]
    pub fn set(&self, snapshot: Option<OrderSnapshot>) {
        *self.inner.lock() = snapshot;
    }
}

// ============================================================================
// CANCELLATION REQUEST CHANNEL (DECOUPLE FROM HOT PATH)
// ============================================================================

/// Cancellation request sent from monitor to cancellation handler
#[derive(Debug)]
pub enum CancelRequest {
    /// Cancel due to age expiry
    AgeExpiry {
        symbol: String,
        reason: String,
        client_order_id: String,
        placed_at: Instant,
    },
    /// Cancel due to profit deviation
    ProfitDeviation {
        symbol: String,
        current_profit_bps: f64,
        deviation_bps: f64,
        client_order_id: String,
        placed_at: Instant,
    },
}

// ============================================================================
// ORDER MONITOR SERVICE (OPTIMIZED)
// ============================================================================

/// Order monitoring service
///
/// Monitors active orders for:
/// 1. Age - refreshes order if age > order_refresh_interval_secs
/// 2. Profit deviation - cancels if profit drops > profit_cancel_threshold_bps
/// 3. Periodic profit logging every 2 seconds
///
/// Key optimizations:
/// - Lock-free status check via atomic
/// - No REST calls in hot loop (delegated to separate task)
/// - No cloning (uses lightweight snapshot)
/// - No allocations in hot path
pub struct OrderMonitorService {
    // Shared state (write lock only needed for mutations)
    pub bot_state: Arc<RwLock<BotState>>,
    
    // Lock-free status for hot path (updated by state manager)
    pub atomic_status: Arc<AtomicU8>,
    
    // Lightweight order snapshot (updated when order placed)
    pub order_snapshot: Arc<SharedOrderSnapshot>,
    
    // Price feeds (lock-free reads via parking_lot)
    pub pacifica_prices: Arc<Mutex<(f64, f64)>>,
    pub hyperliquid_prices: Arc<Mutex<(f64, f64)>>,
    
    // Configuration
    pub config: Config,
    pub maker_symbol: String,
    pub hedge_symbol: String,
    pub evaluator: OpportunityEvaluator,
    
    // Trading connectors (only used by cancellation task)
    pub maker_exchange: Arc<dyn MakerExchange>,
    pub hyperliquid_trading: Arc<HyperliquidTrading>,
    
    // Channel for cancel requests (decouples hot path from I/O)
    pub cancel_tx: mpsc::Sender<CancelRequest>,
}

impl OrderMonitorService {
    /// Create a new order monitor service with cancellation channel
    pub fn new(
        bot_state: Arc<RwLock<BotState>>,
        atomic_status: Arc<AtomicU8>,
        order_snapshot: Arc<SharedOrderSnapshot>,
        pacifica_prices: Arc<Mutex<(f64, f64)>>,
        hyperliquid_prices: Arc<Mutex<(f64, f64)>>,
        config: Config,
        maker_symbol: String,
        hedge_symbol: String,
        evaluator: OpportunityEvaluator,
        maker_exchange: Arc<dyn MakerExchange>,
        hyperliquid_trading: Arc<HyperliquidTrading>,
    ) -> (Self, mpsc::Receiver<CancelRequest>) {
        // Bounded channel to prevent unbounded growth, but large enough to not block
        let (cancel_tx, cancel_rx) = mpsc::channel(64);
        
        let service = Self {
            bot_state,
            atomic_status,
            order_snapshot,
            pacifica_prices,
            hyperliquid_prices,
            config,
            maker_symbol,
            hedge_symbol,
            evaluator,
            maker_exchange,
            hyperliquid_trading,
            cancel_tx,
        };
        
        (service, cancel_rx)
    }

    /// Main monitoring loop - LATENCY CRITICAL
    /// 
    /// This loop runs at 1kHz and must complete each iteration in <1ms.
    /// All I/O operations are delegated to separate tasks via channels.
    pub async fn run_monitor_loop(&self) {
        let mut monitor_interval = interval(Duration::from_millis(1));
        
        // Reduce churn: once age threshold is reached, wait 3s then evaluate.
        let age_cancel_decision_grace = Duration::from_secs(3);
        // Keep waiting if current edge drift is still small.
        let age_cancel_keep_waiting_deviation_bps = 10.0_f64;
        let mut tracked_client_order_id: Option<String> = None;
        let mut age_threshold_reached_at: Option<Instant> = None;
        let mut age_cancel_sent_for_current_order = false;

        loop {
            monitor_interval.tick().await;

            // FAST PATH: Lock-free status check
            let status = self.atomic_status.load(Ordering::Acquire);
            if status != AtomicBotStatus::OrderPlaced as u8 {
                continue;
            }

            // Get order snapshot (single lock, no clone of complex types)
            let snapshot = match self.order_snapshot.get() {
                Some(s) => s,
                None => continue,
            };
            if tracked_client_order_id
                .as_deref()
                != Some(snapshot.client_order_id.as_str())
            {
                tracked_client_order_id = Some(snapshot.client_order_id.clone());
                age_threshold_reached_at = None;
                age_cancel_sent_for_current_order = false;
            }

            // Get prices (parking_lot mutex is very fast for uncontended case)
            let (hl_bid, hl_ask) = *self.hyperliquid_prices.lock();
            if hl_bid == 0.0 || hl_ask == 0.0 {
                continue;
            }

            let age = snapshot.placed_at.elapsed();

            // Check 2: Profit deviation (using raw method - no allocation)
            let current_profit = self.evaluator.recalculate_profit_raw(
                snapshot.side,
                snapshot.price,
                hl_bid,
                hl_ask,
            );
            
            // Cancel only when profit has DROPPED (not when it improved).
            // profit_drop > 0 means the opportunity got worse.
            let profit_drop = snapshot.initial_profit_bps - current_profit;
            let age_threshold = Duration::from_secs(snapshot.order_refresh_interval_secs);
            let age_cancel_hard_cap = age_threshold + Duration::from_secs(120);
            let profit_threshold = snapshot.profit_cancel_threshold_bps;

            // Check 1: Age threshold with anti-churn logic
            if age > age_threshold {
                let since = age_threshold_reached_at.get_or_insert_with(Instant::now);
                let decision_elapsed = since.elapsed();
                let should_keep_waiting = profit_drop.abs() <= age_cancel_keep_waiting_deviation_bps;
                let hit_hard_cap = age >= age_cancel_hard_cap;

                if age_cancel_sent_for_current_order {
                    continue;
                }

                if decision_elapsed < age_cancel_decision_grace {
                    continue;
                }

                if should_keep_waiting && !hit_hard_cap {
                    continue;
                }

                let reason = if hit_hard_cap {
                    format!(
                        "age hard cap {}s reached (age={}ms, deviation={:.2} bps)",
                        age_cancel_hard_cap.as_secs(),
                        age.as_millis(),
                        profit_drop.abs()
                    )
                } else {
                    format!(
                        "age {}ms > {}s threshold and deviation {:.2} bps > {:.2} bps",
                        age.as_millis(),
                        snapshot.order_refresh_interval_secs,
                        profit_drop.abs(),
                        age_cancel_keep_waiting_deviation_bps
                    )
                };

                let _ = self.cancel_tx.try_send(CancelRequest::AgeExpiry {
                    symbol: self.maker_symbol.clone(),
                    reason,
                    client_order_id: snapshot.client_order_id.clone(),
                    placed_at: snapshot.placed_at,
                });
                age_cancel_sent_for_current_order = true;
                continue;
            } else {
                age_threshold_reached_at = None;
            }

            // Check 2: Profit deviation - only cancel when profit DROPPED
            if profit_drop > profit_threshold {
                // Send cancel request (non-blocking)
                let _ = self.cancel_tx.try_send(CancelRequest::ProfitDeviation {
                    symbol: self.maker_symbol.clone(),
                    current_profit_bps: current_profit,
                    deviation_bps: profit_drop,
                    client_order_id: snapshot.client_order_id.clone(),
                    placed_at: snapshot.placed_at,
                });
            }
        }
    }

    /// Cancellation handler task - runs separately from hot path
    /// 
    /// Handles all I/O operations: REST API calls, state updates, logging
    pub async fn run_cancellation_handler(
        &self,
        mut cancel_rx: mpsc::Receiver<CancelRequest>,
    ) {
        let mut rate_limit = RateLimitTracker::new();

        while let Some(request) = cancel_rx.recv().await {
            // Check rate limit backoff
            if rate_limit.should_skip() {
                debug!(
                    "[CANCEL] Skipping cancellation (rate limit backoff, {:.1}s remaining)",
                    rate_limit.remaining_backoff_secs()
                );
                continue;
            }

            // Double-check state hasn't changed (order might have filled)
            let status = self.atomic_status.load(Ordering::Acquire);
            if status != AtomicBotStatus::OrderPlaced as u8 {
                debug!("[CANCEL] Skipping - status changed to {}", status);
                continue;
            }

            // Skip stale cancel requests (must match current order id + placed_at)
            let current_snapshot = match self.order_snapshot.get() {
                Some(s) => s,
                None => {
                    debug!("[CANCEL] Skipping - no active order snapshot");
                    continue;
                }
            };
            let (req_symbol, req_client_order_id, req_placed_at) = match &request {
                CancelRequest::AgeExpiry {
                    symbol,
                    client_order_id,
                    placed_at,
                    ..
                } => (symbol.as_str(), client_order_id.as_str(), *placed_at),
                CancelRequest::ProfitDeviation {
                    symbol,
                    client_order_id,
                    placed_at,
                    ..
                } => (symbol.as_str(), client_order_id.as_str(), *placed_at),
            };
            if current_snapshot.client_order_id != req_client_order_id
                || current_snapshot.placed_at != req_placed_at
            {
                debug!(
                    "[CANCEL] Skipping stale request for {} (req={}, current={})",
                    req_symbol,
                    req_client_order_id,
                    current_snapshot.client_order_id
                );
                continue;
            }

            // Check for partial fills before cancelling
            match self.check_for_fills(req_client_order_id).await {
                FillCheckResult::HasFills(amount) => {
                    info!(
                        "[CANCEL] Order has fills ({}) - skipping cancellation, waiting for fill detection",
                        amount
                    );
                    continue;
                }
                FillCheckResult::NotFound => {
                    debug!("[CANCEL] Order not in open orders - might be filled/cancelled");
                    continue;
                }
                FillCheckResult::NoFills => {
                    // Safe to proceed with cancellation
                }
                FillCheckResult::CheckFailed(e) => {
                    debug!("[CANCEL] Fill check failed: {} - proceeding with cancellation", e);
                    // Continue with cancellation (safer than leaving hanging orders)
                }
            }

            // Log the cancellation reason
            match &request {
                CancelRequest::AgeExpiry {
                    reason,
                    client_order_id,
                    ..
                } => {
                    info!("[CANCEL] Age expiry: {}", reason);
                    debug!("[CANCEL] target cloid={}", client_order_id);
                }
                CancelRequest::ProfitDeviation {
                    current_profit_bps,
                    deviation_bps,
                    client_order_id,
                    ..
                } => {
                    info!(
                        "[CANCEL] Profit deviation: current={:.2} bps, deviation={:.2} bps",
                        current_profit_bps, deviation_bps
                    );
                    debug!("[CANCEL] target cloid={}", client_order_id);
                }
            }

            // Execute cancellation
            let symbol = match &request {
                CancelRequest::AgeExpiry { symbol, .. } => symbol.as_str(),
                CancelRequest::ProfitDeviation { symbol, .. } => symbol.as_str(),
            };

            match self.maker_exchange.cancel_all_orders(Some(symbol)).await {
                Ok(_) => {
                    rate_limit.record_success();
                    
                    // Clear state only if still in OrderPlaced
                    let mut state = self.bot_state.write().await;
                    if matches!(state.status, BotStatus::OrderPlaced)
                        && state
                            .active_order
                            .as_ref()
                            .map(|o| o.client_order_id.as_str() == req_client_order_id)
                            .unwrap_or(false)
                    {
                        state.mark_cancel_requested_for_symbol(symbol);
                        state.clear_active_order();
                        self.atomic_status.store(AtomicBotStatus::Idle as u8, Ordering::Release);
                        self.order_snapshot.set(None);
                    }
                    drop(state);

                    // Refresh prices in parallel (not blocking the handler)
                    self.refresh_prices_parallel().await;
                }
                Err(e) => {
                    if is_rate_limit_error(&e) {
                        rate_limit.record_error();
                        warn!(
                            "[CANCEL] Rate limit exceeded. Backing off for {}s (attempt #{})",
                            rate_limit.get_backoff_secs(),
                            rate_limit.consecutive_errors()
                        );
                    } else {
                        warn!("[CANCEL] Failed to cancel: {}", e);
                    }
                }
            }
        }
    }

    /// Periodic profit logging task - runs at 0.5 Hz (every 2 seconds)
    pub async fn run_profit_logger(&self) {
        let mut log_interval = interval(Duration::from_secs(2));

        loop {
            log_interval.tick().await;

            // Only log for active orders
            let status = self.atomic_status.load(Ordering::Acquire);
            if status != AtomicBotStatus::OrderPlaced as u8 {
                continue;
            }

            let snapshot = match self.order_snapshot.get() {
                Some(s) => s,
                None => continue,
            };

            let (hl_bid, hl_ask) = *self.hyperliquid_prices.lock();
            if hl_bid == 0.0 || hl_ask == 0.0 {
                continue;
            }

            let current_profit = self.evaluator.recalculate_profit_raw(
                snapshot.side,
                snapshot.price,
                hl_bid,
                hl_ask,
            );
            let profit_change = current_profit - snapshot.initial_profit_bps;
            let age_ms = snapshot.placed_at.elapsed().as_millis();

            let hedge_price = match snapshot.side {
                OrderSide::Buy => hl_bid,
                OrderSide::Sell => hl_ask,
            };

            info!(
                "[PROFIT] Current: {:.2} bps (initial: {:.2}, change: {:+.2}) | PAC: ${:.4} | HL: ${:.4} | Age: {:.3}s",
                current_profit,
                snapshot.initial_profit_bps,
                profit_change,
                snapshot.price,
                hedge_price,
                age_ms as f64 / 1000.0
            );
        }
    }

    /// Check if order has fills (called from cancellation handler, not hot path)
    async fn check_for_fills(&self, client_order_id: &str) -> FillCheckResult {
        match self.maker_exchange.get_open_orders(Some(&self.maker_symbol)).await {
            Ok(orders) => {
                if let Some(order) = orders
                    .iter()
                    .find(|o| o.client_order_id.as_str() == client_order_id)
                {
                    let filled_amount = order.filled_amount;
                    if filled_amount > 0.0 {
                        FillCheckResult::HasFills(filled_amount)
                    } else {
                        FillCheckResult::NoFills
                    }
                } else {
                    FillCheckResult::NotFound
                }
            }
            Err(e) => FillCheckResult::CheckFailed(e.to_string()),
        }
    }

    /// Refresh prices from both exchanges in parallel
    async fn refresh_prices_parallel(&self) {
        let pac_future = self.maker_exchange.get_best_bid_ask_rest(
            &self.maker_symbol,
            self.config.agg_level,
        );
        let hl_future = self.hyperliquid_trading.get_l2_snapshot(&self.hedge_symbol);

        let (pac_result, hl_result) = tokio::join!(pac_future, hl_future);

        if let Ok(Some((bid, ask))) = pac_result {
            *self.pacifica_prices.lock() = (bid, ask);
            debug!(
                "[REFRESH] {}: bid=${:.6}, ask=${:.6}",
                self.maker_exchange.name(),
                bid,
                ask
            );
        }

        if let Ok(Some((bid, ask))) = hl_result {
            *self.hyperliquid_prices.lock() = (bid, ask);
            debug!("[REFRESH] Hyperliquid: bid=${:.6}, ask=${:.6}", bid, ask);
        }
    }
}

/// Result of checking for partial fills
enum FillCheckResult {
    HasFills(f64),
    NoFills,
    NotFound,
    CheckFailed(String),
}

// ============================================================================
// HELPER: Update atomic status when BotState changes
// ============================================================================

/// Call this whenever BotState.status changes to keep atomic in sync
#[inline]
pub fn sync_atomic_status(atomic: &AtomicU8, status: &BotStatus) {
    let atomic_val = AtomicBotStatus::from(status) as u8;
    atomic.store(atomic_val, Ordering::Release);
}

/// Call this when placing a new order to update snapshot
#[inline]
pub fn update_order_snapshot(
    snapshot: &SharedOrderSnapshot,
    client_order_id: String,
    side: OrderSide,
    price: f64,
    size: f64,
    initial_profit_bps: f64,
    placed_at: Instant,
    profit_cancel_threshold_bps: f64,
    order_refresh_interval_secs: u64,
) {
    snapshot.set(Some(OrderSnapshot {
        client_order_id,
        side,
        price,
        size,
        initial_profit_bps,
        placed_at,
        profit_cancel_threshold_bps,
        order_refresh_interval_secs,
    }));
}

// ============================================================================
// STARTUP HELPER
// ============================================================================

/// Spawn all monitor tasks
pub fn spawn_monitor_tasks(service: Arc<OrderMonitorService>, cancel_rx: mpsc::Receiver<CancelRequest>) {
    // Hot path monitor (1kHz)
    let service_clone = Arc::clone(&service);
    tokio::spawn(async move {
        service_clone.run_monitor_loop().await;
    });

    // Cancellation handler (processes cancel requests)
    let service_clone = Arc::clone(&service);
    tokio::spawn(async move {
        service_clone.run_cancellation_handler(cancel_rx).await;
    });

    // Profit logger (0.5 Hz)
    let service_clone = Arc::clone(&service);
    tokio::spawn(async move {
        service_clone.run_profit_logger().await;
    });
}
