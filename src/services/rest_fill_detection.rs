use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use colored::Colorize;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

use crate::audit_logger;
use crate::bot::{ActiveOrder, BotState, PendingOrder};
use crate::connector::maker::MakerExchange;
use crate::services::HedgeEvent;
use crate::strategy::OrderSide;
use crate::util::api_health::BinanceApiHealth;
use crate::util::rate_limit::{is_rate_limit_error, parse_ban_until_ms};

/// REST API fill detection service (backup/fallback method)
///
/// Polls open orders via maker REST API to detect fills that may have been
/// missed by primary streaming methods.
pub struct RestFillDetectionService {
    pub bot_state: Arc<RwLock<BotState>>,
    pub hedge_tx: mpsc::UnboundedSender<HedgeEvent>,
    pub maker_exchange: Arc<dyn MakerExchange>,
    pub symbol: String,
    pub processed_fills: Arc<parking_lot::Mutex<HashSet<String>>>,
    pub min_hedge_notional: f64,
    pub poll_interval_ms: u64,
    pub api_health: Arc<BinanceApiHealth>,
}

impl RestFillDetectionService {
    fn is_fill_already_processed(
        &self,
        exchange_order_id: Option<&str>,
        client_order_id: Option<&str>,
    ) -> bool {
        let processed = self.processed_fills.lock();
        let mut keys = Vec::new();
        if let Some(order_id) = exchange_order_id {
            keys.push(format!("full_{}", order_id));
        }
        if let Some(cloid) = client_order_id {
            keys.push(format!("full_{}", cloid));
        }
        keys.iter().any(|k| processed.contains(k))
    }

    fn mark_fill_processed(
        &self,
        exchange_order_id: Option<&str>,
        client_order_id: Option<&str>,
        is_full_fill: bool,
        source_tag: &str,
    ) {
        let mut processed = self.processed_fills.lock();

        if let Some(order_id) = exchange_order_id {
            processed.insert(format!(
                "{}_{}",
                if is_full_fill { "full" } else { "partial" },
                order_id
            ));
        }
        if let Some(cloid) = client_order_id {
            processed.insert(format!(
                "{}_{}",
                if is_full_fill { "full" } else { "partial" },
                cloid
            ));
        }

        if let Some(cloid) = client_order_id {
            processed.insert(format!(
                "{}_{}_{}",
                if is_full_fill { "full" } else { "partial" },
                cloid,
                source_tag
            ));
        }
        if let Some(order_id) = exchange_order_id {
            processed.insert(format!(
                "{}_{}_{}",
                if is_full_fill { "full" } else { "partial" },
                order_id,
                source_tag
            ));
        }
    }

    fn tracked_order_key(order: &ActiveOrder) -> String {
        order
            .exchange_order_id
            .clone()
            .unwrap_or_else(|| order.client_order_id.clone())
    }

    fn open_order_matches_tracked(
        tracked: &ActiveOrder,
        open: &crate::connector::maker::MakerOpenOrder,
    ) -> bool {
        open.client_order_id == tracked.client_order_id
            || tracked
                .exchange_order_id
                .as_ref()
                .map(|id| open.order_id.as_deref() == Some(id.as_str()))
                .unwrap_or(false)
    }

    async fn emit_fill_and_trigger_hedge(
        &self,
        order_side: OrderSide,
        fill_size: f64,
        fill_price: f64,
        exchange_order_id: Option<String>,
        client_order_id: Option<String>,
        is_full_fill: bool,
        source: &str,
        filled_order: Option<ActiveOrder>,
    ) {
        let fills_csv = audit_logger::fill_file_for_symbol(&self.symbol);
        let fill_record = audit_logger::FillRecord::new(
            chrono::Utc::now(),
            self.symbol.clone(),
            self.maker_exchange.name().to_string(),
            order_side,
            fill_size,
            fill_price,
            is_full_fill,
            client_order_id.clone(),
            exchange_order_id.clone(),
            source.to_string(),
        );
        if let Err(e) = audit_logger::log_fill(&fills_csv, &fill_record) {
            debug!("[REST_FILL_DETECTION] Failed to log fill CSV: {}", e);
        }

        {
            let mut state = self.bot_state.write().await;
            if let Some(order) = filled_order {
                state.active_order = Some(order);
            }
            state.mark_filled(fill_size, order_side);
        }

        info!(
            "{} {} State updated to Filled ({})",
            "[REST_FILL_DETECTION]".bright_cyan().bold(),
            "✓".green().bold(),
            source
        );

        info!(
            "{} {} Cancelling maker orders...",
            "[REST_FILL_DETECTION]".bright_cyan().bold(),
            "⚡".yellow().bold()
        );

        match self.maker_exchange.cancel_all_orders(Some(&self.symbol)).await {
            Ok(count) => {
                info!(
                    "{} {} Cancellation complete ({} order(s))",
                    "[REST_FILL_DETECTION]".bright_cyan().bold(),
                    "✓".green().bold(),
                    count
                );
            }
            Err(e) => {
                error!(
                    "{} {} Cancellation failed: {}",
                    "[REST_FILL_DETECTION]".bright_cyan().bold(),
                    "✗".red().bold(),
                    e
                );
            }
        }

        info!(
            "{} {}, triggering hedge ({})",
            format!("[{}]", self.symbol).bright_white().bold(),
            "Order filled".green().bold(),
            source
        );

        let _ = self
            .hedge_tx
            .send((order_side, fill_size, fill_price, Instant::now()));
    }

    async fn recover_fill_via_trade_history(
        &self,
        order_id: &str,
        client_order_id: Option<&str>,
        order_side: OrderSide,
    ) -> Result<bool> {
        if self.is_fill_already_processed(Some(order_id), client_order_id) {
            return Ok(false);
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let start_ms = now_ms.saturating_sub(10 * 60 * 1000);

        let trades = self
            .maker_exchange
            .get_trade_history(Some(&self.symbol), Some(200), Some(start_ms), Some(now_ms))
            .await?;

        let matched: Vec<_> = trades
            .iter()
            .filter(|t| t.is_maker_fill && t.order_id.as_deref() == Some(order_id))
            .collect();

        if matched.is_empty() {
            return Ok(false);
        }

        let fill_size: f64 = matched.iter().map(|t| t.amount).sum();
        let fill_notional: f64 = matched.iter().map(|t| t.amount * t.entry_price).sum();
        if fill_size <= 0.0 || fill_notional <= 0.0 {
            return Ok(false);
        }
        if fill_notional < self.min_hedge_notional {
            debug!(
                "[REST_FILL_DETECTION] Recovered fill notional ${:.2} < ${:.2}, skip hedge",
                fill_notional, self.min_hedge_notional
            );
            return Ok(false);
        }

        let fill_price = fill_notional / fill_size;

        info!(
            "{} {} Recovered FILL via trade history: {} {} @ {} | Notional: ${:.2} | order_id={}",
            "[REST_FILL_DETECTION]".bright_cyan().bold(),
            "✓".green().bold(),
            order_side.as_str().bright_yellow(),
            format!("{:.6}", fill_size).bright_white(),
            format!("${:.6}", fill_price).cyan(),
            fill_notional,
            order_id
        );

        self.mark_fill_processed(Some(order_id), client_order_id, true, "history");
        self.emit_fill_and_trigger_hedge(
            order_side,
            fill_size,
            fill_price,
            Some(order_id.to_string()),
            client_order_id.map(ToString::to_string),
            true,
            "rest_trade_history_recovery",
            None,
        )
        .await;

        Ok(true)
    }

    pub async fn run(self) {
        let mut consecutive_errors = 0u32;
        let mut last_known_filled_amounts: HashMap<String, f64> = HashMap::new();
        let mut hedged_filled_amounts: HashMap<String, f64> = HashMap::new();

        loop {
            let has_pending_orders = {
                let state = self.bot_state.read().await;
                !state.pending_orders_for_symbol(&self.symbol).is_empty()
                    || matches!(
                        state.status,
                        crate::bot::BotStatus::Filled | crate::bot::BotStatus::Hedging
                    )
            };

            let poll_ms = if has_pending_orders {
                self.poll_interval_ms
            } else {
                1000
            };
            tokio::time::sleep(Duration::from_millis(poll_ms)).await;

            let tracked_orders: Vec<PendingOrder> = {
                let state = self.bot_state.read().await;

                if matches!(
                    state.status,
                    crate::bot::BotStatus::Complete | crate::bot::BotStatus::Error(_)
                ) {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                let pending = state.pending_orders_for_symbol(&self.symbol);
                if !pending.is_empty() {
                    pending
                } else if matches!(
                    state.status,
                    crate::bot::BotStatus::Filled | crate::bot::BotStatus::Hedging
                ) {
                    Vec::new()
                } else {
                    last_known_filled_amounts.clear();
                    hedged_filled_amounts.clear();
                    continue;
                }
            };

            let tracked_keys: HashSet<String> = tracked_orders
                .iter()
                .map(|p| Self::tracked_order_key(&p.order))
                .collect();
            last_known_filled_amounts.retain(|k, _| tracked_keys.contains(k));
            hedged_filled_amounts.retain(|k, _| tracked_keys.contains(k));

            if tracked_orders.is_empty() {
                continue;
            }

            let open_orders_result = self.maker_exchange.get_open_orders(Some(&self.symbol)).await;

            match open_orders_result {
                Ok(orders) => {
                    consecutive_errors = 0;
                    self.api_health.mark_rest_available(true);

                    for tracked in tracked_orders {
                        let tracked_order = tracked.order.clone();
                        let tracked_key = Self::tracked_order_key(&tracked_order);
                        let last_known = *last_known_filled_amounts.get(&tracked_key).unwrap_or(&0.0);

                        let maybe_open_order = orders
                            .iter()
                            .find(|o| Self::open_order_matches_tracked(&tracked_order, o))
                            .cloned();

                        if let Some(open_order) = maybe_open_order {
                            let filled_amount = open_order.filled_amount;
                            let initial_amount = open_order.initial_amount;
                            let price = open_order.price;
                            let hedged_so_far =
                                *hedged_filled_amounts.get(&tracked_key).unwrap_or(&0.0);

                            if filled_amount > last_known && filled_amount > 0.0 {
                                let new_fill_amount = filled_amount - last_known;
                                let new_fill_notional = new_fill_amount * price;
                                // Hedge against cumulative unhedged exposure rather than just the latest delta.
                                let hedge_fill_amount = (filled_amount - hedged_so_far).max(0.0);
                                let hedge_notional = hedge_fill_amount * price;

                                debug!(
                                    "[REST_FILL_DETECTION] Fill detected: {} -> {} (new: {}, unhedged: {}) | Notional(new): ${:.2}, Notional(unhedged): ${:.2} | cloid={}",
                                    last_known,
                                    filled_amount,
                                    new_fill_amount,
                                    hedge_fill_amount,
                                    new_fill_notional,
                                    hedge_notional,
                                    tracked_order.client_order_id
                                );

                                last_known_filled_amounts.insert(tracked_key.clone(), filled_amount);
                                let is_full_fill = (filled_amount - initial_amount).abs() < 0.0001
                                    || (filled_amount + 1e-9) >= tracked_order.size;

                                if is_full_fill || hedge_notional > self.min_hedge_notional {
                                    if hedge_fill_amount <= 0.0 {
                                        debug!(
                                            "[REST_FILL_DETECTION] Unhedged amount is non-positive ({}), skipping hedge trigger",
                                            hedge_fill_amount
                                        );
                                        continue;
                                    }

                                    let cloid = &open_order.client_order_id;
                                    if self.is_fill_already_processed(
                                        open_order.order_id.as_deref(),
                                        Some(cloid),
                                    ) {
                                        debug!("[REST_FILL_DETECTION] Fill already processed, skipping");
                                        continue;
                                    }

                                    self.mark_fill_processed(
                                        open_order.order_id.as_deref(),
                                        Some(cloid),
                                        is_full_fill,
                                        "rest",
                                    );

                                    info!(
                                        "{} {} {} FILL: {} {} {} @ {} | Filled: {} / {} | Notional: {} {}",
                                        "[REST_FILL_DETECTION]".bright_cyan().bold(),
                                        "✓".green().bold(),
                                        if is_full_fill { "FULL" } else { "PARTIAL" },
                                        tracked_order.side.as_str().bright_yellow(),
                                        filled_amount,
                                        self.symbol.bright_white().bold(),
                                        format!("${:.6}", price).cyan(),
                                        filled_amount,
                                        initial_amount,
                                        format!("${:.2}", hedge_notional).cyan().bold(),
                                        "(REST API)".bright_black()
                                    );

                                    if is_full_fill {
                                        let mut state = self.bot_state.write().await;
                                        state.remove_pending_order(
                                            Some(&tracked_order.client_order_id),
                                            tracked_order.exchange_order_id.as_deref(),
                                        );
                                        last_known_filled_amounts.remove(&tracked_key);
                                        hedged_filled_amounts.remove(&tracked_key);
                                    }

                                    self.emit_fill_and_trigger_hedge(
                                        tracked_order.side,
                                        hedge_fill_amount,
                                        price,
                                        open_order.order_id.clone(),
                                        Some(open_order.client_order_id.clone()),
                                        is_full_fill,
                                        "rest_fill_detection",
                                        Some(tracked_order.clone()),
                                    )
                                    .await;
                                    if !is_full_fill {
                                        hedged_filled_amounts.insert(tracked_key.clone(), filled_amount);
                                    }
                                } else {
                                    debug!(
                                        "[REST_FILL_DETECTION] Fill notional ${:.2} < ${:.2} threshold, skipping",
                                        hedge_notional, self.min_hedge_notional
                                    );
                                }
                            }
                            continue;
                        }

                        if let Some(order_id) = tracked_order.exchange_order_id.as_deref() {
                            match self
                                .recover_fill_via_trade_history(
                                    order_id,
                                    Some(&tracked_order.client_order_id),
                                    tracked_order.side,
                                )
                                .await
                            {
                                Ok(true) => {
                                    last_known_filled_amounts.remove(&tracked_key);
                                    hedged_filled_amounts.remove(&tracked_key);
                                    let mut state = self.bot_state.write().await;
                                    state.remove_pending_order(
                                        Some(&tracked_order.client_order_id),
                                        Some(order_id),
                                    );
                                    continue;
                                }
                                Ok(false) => {}
                                Err(e) => debug!(
                                    "[REST_FILL_DETECTION] Trade history recovery failed (cloid={}): {}",
                                    tracked_order.client_order_id, e
                                ),
                            }
                        }

                        debug!(
                            "[REST_FILL_DETECTION] Pending order not found in open orders: cloid={} oid={} cancel_requested={}",
                            tracked_order.client_order_id,
                            tracked_order.exchange_order_id.as_deref().unwrap_or("-"),
                            tracked.cancel_requested
                        );
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    let is_rate_limit = is_rate_limit_error(&e);

                    if is_rate_limit {
                        self.api_health.mark_rest_available(false);
                        if let Some(until_ms) = parse_ban_until_ms(&e) {
                            self.api_health.mark_banned(until_ms);
                            warn!(
                                "{} {} IP banned until {} - pausing REST polling",
                                "[REST_FILL_DETECTION]".bright_cyan().bold(),
                                "⚠".yellow().bold(),
                                until_ms
                            );
                        }
                        let backoff_secs = std::cmp::min(2u64.pow(consecutive_errors - 1), 32);
                        warn!(
                            "{} {} Rate limit hit, backing off for {} seconds...",
                            "[REST_FILL_DETECTION]".bright_cyan().bold(),
                            "⚠".yellow().bold(),
                            backoff_secs
                        );
                        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    } else {
                        debug!(
                            "[REST_FILL_DETECTION] Error fetching open orders (attempt {}): {}",
                            consecutive_errors, e
                        );
                        if consecutive_errors >= 5 {
                            self.api_health.mark_rest_available(false);
                            warn!(
                                "{} {} {} consecutive errors fetching open orders",
                                "[REST_FILL_DETECTION]".bright_cyan().bold(),
                                "⚠".yellow().bold(),
                                consecutive_errors
                            );
                        }
                    }
                }
            }
        }
    }
}
