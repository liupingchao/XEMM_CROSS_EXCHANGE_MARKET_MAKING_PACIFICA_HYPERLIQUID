use crate::strategy::OrderSide;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Bot status enumeration
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotStatus {
    /// Bot is idle, waiting for an opportunity
    Idle,
    /// Order has been placed on maker exchange
    OrderPlaced,
    /// Order has been filled on maker exchange
    Filled,
    /// Hedge is being executed on Hyperliquid
    Hedging,
    /// Full cycle complete (order filled + hedged)
    Complete,
    /// Error occurred
    Error(String),
}

/// Active order information
#[derive(Debug, Clone)]
pub struct ActiveOrder {
    /// Client order ID
    pub client_order_id: String,
    /// Exchange-native order ID if available
    pub exchange_order_id: Option<String>,
    /// Trading symbol (e.g., "SOL")
    pub symbol: String,
    /// Order side (Buy or Sell)
    pub side: OrderSide,
    /// Limit price
    pub price: f64,
    /// Order size
    pub size: f64,
    /// Initial calculated profit in basis points
    pub initial_profit_bps: f64,
    /// When the order was placed
    pub placed_at: Instant,
}

/// Order tracked for asynchronous fill/cancel reconciliation.
#[derive(Debug, Clone)]
pub struct PendingOrder {
    /// Original order payload tracked by the bot.
    pub order: ActiveOrder,
    /// Whether cancellation was requested for this order.
    pub cancel_requested: bool,
}

impl PendingOrder {
    fn matches_ids(&self, client_order_id: Option<&str>, exchange_order_id: Option<&str>) -> bool {
        if client_order_id.is_none() && exchange_order_id.is_none() {
            return false;
        }

        let client_match = client_order_id
            .map(|id| self.order.client_order_id == id)
            .unwrap_or(false);
        let exchange_match = exchange_order_id
            .map(|id| self.order.exchange_order_id.as_deref() == Some(id))
            .unwrap_or(false);
        client_match || exchange_match
    }
}

/// Normal-mode carry position tracking.
#[derive(Debug, Clone)]
pub struct NormalModePosition {
    /// Mid spread in bps when the position was opened.
    pub entry_spread_bps: f64,
    /// When the position was opened.
    pub entry_time: Instant,
    /// Accumulated size (positive = short maker + long hedge).
    pub size: f64,
}

/// Bot state (thread-safe via Arc<RwLock<BotState>>)
#[derive(Debug)]
pub struct BotState {
    /// Currently active order (if any)
    pub active_order: Option<ActiveOrder>,
    /// All recently placed maker orders waiting for final fill/cancel confirmation.
    pub pending_orders: Vec<PendingOrder>,
    /// Current position size (+ for long, - for short, 0 for flat)
    pub position: f64,
    /// Current bot status
    pub status: BotStatus,
    /// Atomic status for fast lock-free checks (0=Idle, 1=OrderPlaced, 2=Filled, 3=Hedging, 4=Complete, 5=Error)
    pub status_atomic: Arc<AtomicU8>,
    /// Last time an order was cancelled (for grace period enforcement)
    pub last_cancellation_time: Option<Instant>,
    /// Normal-mode carry position (Some when holding a carry position).
    pub normal_position: Option<NormalModePosition>,
}

impl BotState {
    /// Create a new bot state in Idle status
    pub fn new() -> Self {
        Self {
            active_order: None,
            pending_orders: Vec::new(),
            position: 0.0,
            status: BotStatus::Idle,
            status_atomic: Arc::new(AtomicU8::new(0)), // 0 = Idle
            last_cancellation_time: None,
            normal_position: None,
        }
    }

    /// Set active order and update status
    pub fn set_active_order(&mut self, order: ActiveOrder) {
        self.upsert_pending_order(order.clone());
        self.active_order = Some(order);
        self.status = BotStatus::OrderPlaced;
        self.status_atomic.store(1, Ordering::Release); // 1 = OrderPlaced
    }

    /// Insert/update a pending tracked order.
    pub fn upsert_pending_order(&mut self, order: ActiveOrder) {
        if let Some(existing) = self
            .pending_orders
            .iter_mut()
            .find(|p| p.matches_ids(Some(&order.client_order_id), order.exchange_order_id.as_deref()))
        {
            existing.order = order;
            existing.cancel_requested = false;
            return;
        }

        self.pending_orders.push(PendingOrder {
            order,
            cancel_requested: false,
        });
    }

    /// Snapshot tracked pending orders for a specific symbol.
    pub fn pending_orders_for_symbol(&self, symbol: &str) -> Vec<PendingOrder> {
        self.pending_orders
            .iter()
            .filter(|p| p.order.symbol.eq_ignore_ascii_case(symbol))
            .cloned()
            .collect()
    }

    /// Find a pending order by client order id or exchange order id.
    pub fn find_pending_order(
        &self,
        client_order_id: Option<&str>,
        exchange_order_id: Option<&str>,
    ) -> Option<PendingOrder> {
        self.pending_orders
            .iter()
            .find(|p| p.matches_ids(client_order_id, exchange_order_id))
            .cloned()
    }

    /// Mark pending orders as cancellation-requested.
    pub fn mark_cancel_requested_for_symbol(&mut self, symbol: &str) {
        for pending in self
            .pending_orders
            .iter_mut()
            .filter(|p| p.order.symbol.eq_ignore_ascii_case(symbol))
        {
            pending.cancel_requested = true;
        }
    }

    /// Remove a pending order by client/exchange id.
    pub fn remove_pending_order(
        &mut self,
        client_order_id: Option<&str>,
        exchange_order_id: Option<&str>,
    ) -> Option<PendingOrder> {
        let idx = self
            .pending_orders
            .iter()
            .position(|p| p.matches_ids(client_order_id, exchange_order_id))?;
        Some(self.pending_orders.remove(idx))
    }

    /// Remove all pending orders for a symbol.
    pub fn clear_pending_orders_for_symbol(&mut self, symbol: &str) {
        self.pending_orders
            .retain(|p| !p.order.symbol.eq_ignore_ascii_case(symbol));
    }

    /// Clear active order and return to Idle
    pub fn clear_active_order(&mut self) {
        self.active_order = None;
        self.status = BotStatus::Idle;
        self.status_atomic.store(0, Ordering::Release); // 0 = Idle
        self.last_cancellation_time = Some(Instant::now());
    }

    /// Mark order as filled
    pub fn mark_filled(&mut self, filled_size: f64, side: OrderSide) {
        self.status = BotStatus::Filled;
        self.status_atomic.store(2, Ordering::Release); // 2 = Filled

        // Update position
        match side {
            OrderSide::Buy => self.position += filled_size,
            OrderSide::Sell => self.position -= filled_size,
        }
    }

    /// Mark as hedging
    pub fn mark_hedging(&mut self) {
        self.status = BotStatus::Hedging;
        self.status_atomic.store(3, Ordering::Release); // 3 = Hedging
    }

    /// Mark as complete
    pub fn mark_complete(&mut self) {
        self.status = BotStatus::Complete;
        self.status_atomic.store(4, Ordering::Release); // 4 = Complete
        self.active_order = None;
        self.pending_orders.clear();
    }

    /// Mark current cycle complete and return to Idle for continuous mode.
    pub fn mark_cycle_complete_continue(&mut self) {
        self.active_order = None;
        self.pending_orders.clear();
        self.status = BotStatus::Idle;
        self.status_atomic.store(0, Ordering::Release); // 0 = Idle
        self.last_cancellation_time = None;
    }

    /// Set error status
    pub fn set_error(&mut self, error: String) {
        self.status = BotStatus::Error(error);
        self.status_atomic.store(5, Ordering::Release); // 5 = Error
    }

    /// Check if bot is in a terminal state
    pub fn is_terminal(&self) -> bool {
        matches!(self.status, BotStatus::Complete | BotStatus::Error(_))
    }

    /// Check if bot is idle
    pub fn is_idle(&self) -> bool {
        self.status == BotStatus::Idle
    }

    /// Fast lock-free check if bot is idle (using atomic status)
    pub fn is_idle_fast(&self) -> bool {
        self.status_atomic.load(Ordering::Acquire) == 0
    }

    /// Fast lock-free check if bot has an active order (OrderPlaced status)
    pub fn has_active_order_fast(&self) -> bool {
        self.status_atomic.load(Ordering::Acquire) == 1
    }

    /// Fast lock-free get current status as u8
    pub fn get_status_atomic(&self) -> u8 {
        self.status_atomic.load(Ordering::Acquire)
    }

    /// Check if the grace period has passed since last cancellation
    /// Returns true if no cancellation or if grace_period_secs has elapsed
    pub fn grace_period_elapsed(&self, grace_period_secs: u64) -> bool {
        match self.last_cancellation_time {
            None => true, // No previous cancellation
            Some(last_cancel) => last_cancel.elapsed().as_secs() >= grace_period_secs,
        }
    }
}

impl Default for BotState {
    fn default() -> Self {
        Self::new()
    }
}
