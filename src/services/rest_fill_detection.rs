use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

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
                    Some((order.client_order_id.clone(), order.side))
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

            let client_order_id_opt = active_order_info.as_ref().map(|(id, _)| id.clone());
            let order_side_opt = active_order_info.as_ref().map(|(_, side)| *side);

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
                                let fill_type = if is_full_fill { "full" } else { "partial" };
                                let cloid = &order.client_order_id;
                                let fill_id = format!("{}_{}_rest", fill_type, cloid);

                                let mut processed = self.processed_fills.lock();
                                if processed.contains(&fill_id)
                                    || processed.contains(&format!("full_{}", cloid))
                                    || processed.contains(&format!("partial_{}", cloid))
                                {
                                    debug!("[REST_FILL_DETECTION] Fill already processed, skipping");
                                    continue;
                                }
                                processed.insert(fill_id);
                                drop(processed);

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

                                let fills_csv = audit_logger::fill_file_for_symbol(&self.symbol);
                                let fill_record = audit_logger::FillRecord::new(
                                    chrono::Utc::now(),
                                    self.symbol.clone(),
                                    self.maker_exchange.name().to_string(),
                                    order_side,
                                    new_fill_amount,
                                    price,
                                    is_full_fill,
                                    Some(order.client_order_id.clone()),
                                    order.order_id.clone(),
                                    "rest_fill_detection".to_string(),
                                );
                                if let Err(e) = audit_logger::log_fill(&fills_csv, &fill_record) {
                                    debug!("[REST_FILL_DETECTION] Failed to log fill CSV: {}", e);
                                }

                                let bot_state_clone = self.bot_state.clone();
                                let hedge_tx_clone = self.hedge_tx.clone();
                                let maker_exchange_clone = self.maker_exchange.clone();
                                let symbol_clone = self.symbol.clone();

                                tokio::spawn(async move {
                                    {
                                        let mut state = bot_state_clone.write().await;
                                        state.mark_filled(filled_amount, order_side);
                                    }

                                    info!(
                                        "{} {} State updated to Filled (REST)",
                                        "[REST_FILL_DETECTION]".bright_cyan().bold(),
                                        "✓".green().bold()
                                    );

                                    info!(
                                        "{} {} Cancelling maker orders...",
                                        "[REST_FILL_DETECTION]".bright_cyan().bold(),
                                        "⚡".yellow().bold()
                                    );

                                    match maker_exchange_clone.cancel_all_orders(Some(&symbol_clone)).await {
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
                                        "{} {}, triggering hedge (REST)",
                                        format!("[{}]", symbol_clone).bright_white().bold(),
                                        "Order filled".green().bold()
                                    );

                                    let _ = hedge_tx_clone.send((
                                        order_side,
                                        filled_amount,
                                        price,
                                        std::time::Instant::now(),
                                    ));
                                });
                            } else {
                                debug!(
                                    "[REST_FILL_DETECTION] Fill notional ${:.2} < ${:.2} threshold, skipping",
                                    notional_value, self.min_hedge_notional
                                );
                            }
                        }
                    } else if client_order_id_opt.is_some() {
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
