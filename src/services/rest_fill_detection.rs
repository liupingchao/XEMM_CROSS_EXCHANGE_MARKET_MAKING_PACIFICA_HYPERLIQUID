use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use colored::Colorize;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

use crate::audit_logger;
use crate::bot::BotState;
use crate::connector::maker::{MakerExchange, MakerOrderSide};
use crate::services::HedgeEvent;
use crate::strategy::OrderSide;
use crate::util::rate_limit::is_rate_limit_error;

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
            keys.push(format!("partial_{}", order_id));
        }
        if let Some(cloid) = client_order_id {
            keys.push(format!("full_{}", cloid));
            keys.push(format!("partial_{}", cloid));
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

    async fn emit_fill_and_trigger_hedge(
        &self,
        order_side: OrderSide,
        fill_size: f64,
        fill_price: f64,
        exchange_order_id: Option<String>,
        client_order_id: Option<String>,
        is_full_fill: bool,
        source: &str,
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
        )
        .await;

        Ok(true)
    }

    pub async fn run(self) {
        let mut consecutive_errors = 0u32;
        let mut last_known_filled_amount: f64 = 0.0;

        loop {
            let has_active_order = {
                let state = self.bot_state.read().await;
                state.has_active_order_fast()
                    || matches!(
                        state.status,
                        crate::bot::BotStatus::Filled | crate::bot::BotStatus::Hedging
                    )
            };

            let poll_ms = if has_active_order {
                self.poll_interval_ms
            } else {
                1000
            };
            tokio::time::sleep(Duration::from_millis(poll_ms)).await;

            let active_order_info = {
                let state = self.bot_state.read().await;

                if matches!(
                    state.status,
                    crate::bot::BotStatus::Complete | crate::bot::BotStatus::Error(_)
                ) {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                if let Some(ref order) = state.active_order {
                    Some((
                        order.client_order_id.clone(),
                        order.exchange_order_id.clone(),
                        order.side,
                    ))
                } else if matches!(
                    state.status,
                    crate::bot::BotStatus::Filled | crate::bot::BotStatus::Hedging
                ) {
                    None
                } else {
                    last_known_filled_amount = 0.0;
                    continue;
                }
            };

            let client_order_id_opt = active_order_info.as_ref().map(|(id, _, _)| id.clone());
            let exchange_order_id_opt = active_order_info
                .as_ref()
                .and_then(|(_, order_id, _)| order_id.clone());
            let order_side_opt = active_order_info.as_ref().map(|(_, _, side)| *side);

            let open_orders_result = self
                .maker_exchange
                .get_open_orders(Some(&self.symbol))
                .await;

            match open_orders_result {
                Ok(orders) => {
                    consecutive_errors = 0;

                    let our_order = if let Some(ref cloid) = client_order_id_opt {
                        orders.iter().find(|o| &o.client_order_id == cloid)
                    } else {
                        debug!(
                            "[REST_FILL_DETECTION] Recovery mode: searching {} orders for filled orders",
                            orders.len()
                        );
                        orders.iter().find(|o| o.filled_amount > 0.0)
                    };

                    if let Some(order) = our_order {
                        let filled_amount = order.filled_amount;
                        let initial_amount = order.initial_amount;
                        let price = order.price;

                        if filled_amount > last_known_filled_amount && filled_amount > 0.0 {
                            let new_fill_amount = filled_amount - last_known_filled_amount;
                            let notional_value = new_fill_amount * price;

                            debug!(
                                "[REST_FILL_DETECTION] Fill detected: {} -> {} (new: {}) | Notional: ${:.2}",
                                last_known_filled_amount, filled_amount, new_fill_amount, notional_value
                            );

                            last_known_filled_amount = filled_amount;
                            let is_full_fill = (filled_amount - initial_amount).abs() < 0.0001;

                            if is_full_fill || notional_value > self.min_hedge_notional {
                                let cloid = &order.client_order_id;

                                if self.is_fill_already_processed(order.order_id.as_deref(), Some(cloid)) {
                                    debug!("[REST_FILL_DETECTION] Fill already processed, skipping");
                                    continue;
                                }
                                self.mark_fill_processed(
                                    order.order_id.as_deref(),
                                    Some(cloid),
                                    is_full_fill,
                                    "rest",
                                );

                                let order_side = if let Some(side) = order_side_opt {
                                    side
                                } else {
                                    match order.side {
                                        MakerOrderSide::Buy => OrderSide::Buy,
                                        MakerOrderSide::Sell => OrderSide::Sell,
                                    }
                                };

                                info!(
                                    "{} {} {} FILL: {} {} {} @ {} | Filled: {} / {} | Notional: {} {}",
                                    "[REST_FILL_DETECTION]".bright_cyan().bold(),
                                    "✓".green().bold(),
                                    if is_full_fill { "FULL" } else { "PARTIAL" },
                                    order.side.as_str().bright_yellow(),
                                    filled_amount,
                                    self.symbol.bright_white().bold(),
                                    format!("${:.6}", price).cyan(),
                                    filled_amount,
                                    initial_amount,
                                    format!("${:.2}", notional_value).cyan().bold(),
                                    "(REST API)".bright_black()
                                );

                                self.emit_fill_and_trigger_hedge(
                                    order_side,
                                    new_fill_amount,
                                    price,
                                    order.order_id.clone(),
                                    Some(order.client_order_id.clone()),
                                    is_full_fill,
                                    "rest_fill_detection",
                                )
                                .await;
                            } else {
                                debug!(
                                    "[REST_FILL_DETECTION] Fill notional ${:.2} < ${:.2} threshold, skipping",
                                    notional_value, self.min_hedge_notional
                                );
                            }
                        }
                    } else if client_order_id_opt.is_some() {
                        if let (Some(order_id), Some(order_side)) =
                            (exchange_order_id_opt.as_deref(), order_side_opt)
                        {
                            match self
                                .recover_fill_via_trade_history(
                                    order_id,
                                    client_order_id_opt.as_deref(),
                                    order_side,
                                )
                                .await
                            {
                                Ok(true) => {
                                    last_known_filled_amount = 0.0;
                                    continue;
                                }
                                Ok(false) => {}
                                Err(e) => debug!(
                                    "[REST_FILL_DETECTION] Trade history recovery failed: {}",
                                    e
                                ),
                            }
                        }
                        debug!("[REST_FILL_DETECTION] Active order not found in open_orders");
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    let is_rate_limit = is_rate_limit_error(&e);

                    if is_rate_limit {
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
