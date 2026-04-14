use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use colored::Colorize;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

use crate::audit_logger;
use crate::bot::{BotState, BotStatus};
use crate::connector::binance::{
    BinanceCredentials, BinanceUserStreamClient, BinanceUserStreamConfig, BinanceUserStreamEvent,
};
use crate::connector::maker::{MakerExchange, MakerOrderSide};
use crate::services::HedgeEvent;
use crate::strategy::OrderSide;
use crate::util::api_health::BinanceApiHealth;

pub struct BinanceFillDetectionService {
    pub bot_state: Arc<RwLock<BotState>>,
    pub hedge_tx: mpsc::UnboundedSender<HedgeEvent>,
    pub maker_exchange: Arc<dyn MakerExchange>,
    pub symbol: String,
    pub processed_fills: Arc<parking_lot::Mutex<HashSet<String>>>,
    pub reconnect_attempts: u32,
    pub ping_interval_secs: u64,
    pub min_hedge_notional: f64,
    pub api_health: Arc<BinanceApiHealth>,
}

impl BinanceFillDetectionService {
    pub async fn run(self) {
        loop {
            // Wait for API to become available if currently banned
            while self.api_health.is_banned() {
                self.api_health.mark_stream_alive(false);
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }

            self.api_health.mark_stream_alive(true);

            match self.run_stream_once().await {
                Ok(()) => break, // Graceful exit
                Err(e) => {
                    self.api_health.mark_stream_alive(false);
                    warn!(
                        "{} {} User stream failed: {} — retrying in 60s",
                        "[BINANCE_FILL]".bright_magenta().bold(),
                        "⚠".yellow().bold(),
                        e
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            }
        }
    }

    async fn run_stream_once(&self) -> Result<()> {
        let creds = BinanceCredentials::from_env()
            .map_err(|e| {
                error!("{} {} Failed to load Binance credentials: {}",
                    "[BINANCE_FILL]".bright_magenta().bold(), "✗".red().bold(), e);
                e
            })?;

        let stream_config = BinanceUserStreamConfig {
            reconnect_attempts: self.reconnect_attempts,
            ping_interval_secs: self.ping_interval_secs,
            listen_key_keepalive_secs: 30 * 60,
        };
        let stream_client = BinanceUserStreamClient::new(creds, false, stream_config)
            .map_err(|e| {
                error!("{} {} Failed to create Binance user stream client: {}",
                    "[BINANCE_FILL]".bright_magenta().bold(), "✗".red().bold(), e);
                e
            })?;

        let bot_state = self.bot_state.clone();
        let hedge_tx = self.hedge_tx.clone();
        let maker_exchange = self.maker_exchange.clone();
        let symbol = self.symbol.clone();
        let processed_fills = self.processed_fills.clone();
        let min_hedge_notional = self.min_hedge_notional;
        let cumulative_fill_state = Arc::new(parking_lot::Mutex::new(HashMap::<String, f64>::new()));
        let cumulative_fill_state_cl = cumulative_fill_state.clone();

        info!(
            "{} Starting Binance user stream fill detection",
            "[BINANCE_FILL]".bright_magenta().bold()
        );

        let run_result = stream_client
            .start(move |event| {
                let bot_state = bot_state.clone();
                let hedge_tx = hedge_tx.clone();
                let maker_exchange = maker_exchange.clone();
                let symbol = symbol.clone();
                let processed_fills = processed_fills.clone();
                let cumulative_fill_state = cumulative_fill_state_cl.clone();

                match event {
                    BinanceUserStreamEvent::Trade {
                        symbol: event_symbol,
                        side,
                        client_order_id,
                        order_id,
                        last_fill_qty: _,
                        cumulative_fill_qty,
                        fill_price,
                        order_status,
                        trade_time_ms: _,
                    } => {
                        tokio::spawn(async move {
                            if !symbol_matches(&event_symbol, &symbol) {
                                return;
                            }

                            let key = client_order_id
                                .clone()
                                .or_else(|| order_id.clone())
                                .unwrap_or_else(|| "unknown".to_string());

                            let tracked_order = {
                                let state = bot_state.read().await;
                                state.find_pending_order(
                                    client_order_id.as_deref(),
                                    order_id.as_deref(),
                                )
                            };

                            let Some(tracked_order) = tracked_order else {
                                return;
                            };
                            let strategy_side = tracked_order.order.side;
                            let expected_order_size = tracked_order.order.size;

                            let incremental_fill = {
                                let mut map = cumulative_fill_state.lock();
                                let prev = map.get(&key).copied().unwrap_or(0.0);
                                let delta = (cumulative_fill_qty - prev).max(0.0);
                                if cumulative_fill_qty > prev {
                                    map.insert(key.clone(), cumulative_fill_qty);
                                }
                                delta
                            };

                            if incremental_fill <= 0.0 {
                                return;
                            }

                            let order_side = match side {
                                MakerOrderSide::Buy => OrderSide::Buy,
                                MakerOrderSide::Sell => OrderSide::Sell,
                            };
                            if order_side != strategy_side {
                                warn!(
                                    "[BINANCE_FILL] Side mismatch for active order: active={:?}, event={:?}",
                                    strategy_side, order_side
                                );
                                return;
                            }

                            let is_full_fill = order_status == "FILLED"
                                || (cumulative_fill_qty + 1e-9) >= expected_order_size;
                            let notional_value = incremental_fill * fill_price;
                            if !is_full_fill && notional_value < min_hedge_notional {
                                debug!(
                                    "[BINANCE_FILL] Incremental fill notional ${:.2} < ${:.2}, skip hedge",
                                    notional_value, min_hedge_notional
                                );
                                return;
                            }

                            let event_fill_id =
                                format!("binance_stream_{}_{}", key, cumulative_fill_qty);
                            {
                                let mut processed = processed_fills.lock();
                                if processed.contains(&event_fill_id) {
                                    return;
                                }
                                processed.insert(event_fill_id);
                                if is_full_fill {
                                    processed.insert(format!("full_{}", key));
                                } else {
                                    processed.insert(format!("partial_{}", key));
                                }
                            }

                            info!(
                                "{} {} {} FILL: {} {} @ {} | cumulative={} | notional=${:.2}",
                                "[BINANCE_FILL]".bright_magenta().bold(),
                                if is_full_fill {
                                    "FULL".green().bold()
                                } else {
                                    "PARTIAL".yellow().bold()
                                },
                                match order_side {
                                    OrderSide::Buy => "BUY",
                                    OrderSide::Sell => "SELL",
                                },
                                format!("{:.6}", incremental_fill).bright_white(),
                                symbol.bright_white().bold(),
                                format!("${:.6}", fill_price).cyan(),
                                format!("{:.6}", cumulative_fill_qty).bright_white(),
                                notional_value
                            );

                            let fills_csv = audit_logger::fill_file_for_symbol(&symbol);
                            let fill_record = audit_logger::FillRecord::new(
                                chrono::Utc::now(),
                                symbol.clone(),
                                maker_exchange.name().to_string(),
                                order_side,
                                incremental_fill,
                                fill_price,
                                is_full_fill,
                                client_order_id.clone(),
                                order_id.clone(),
                                "binance_user_stream".to_string(),
                            );
                            if let Err(e) = audit_logger::log_fill(&fills_csv, &fill_record) {
                                debug!("[BINANCE_FILL] Failed to log fill CSV: {}", e);
                            }

                            {
                                let mut state = bot_state.write().await;
                                if matches!(state.status, BotStatus::Complete | BotStatus::Error(_)) {
                                    return;
                                }
                                if is_full_fill {
                                    state.remove_pending_order(
                                        client_order_id.as_deref(),
                                        order_id.as_deref(),
                                    );
                                }
                                state.active_order = Some(tracked_order.order.clone());
                                state.mark_filled(incremental_fill, order_side);
                            }

                            let maker_exchange_bg = maker_exchange.clone();
                            let symbol_bg = symbol.clone();
                            tokio::spawn(async move {
                                if let Err(e) = maker_exchange_bg.cancel_all_orders(Some(&symbol_bg)).await {
                                    warn!("[BINANCE_FILL] Failed to cancel maker orders after fill: {}", e);
                                }
                            });

                            let _ = hedge_tx.send((order_side, incremental_fill, fill_price, Instant::now()));
                        });
                    }
                    BinanceUserStreamEvent::Cancelled {
                        symbol: event_symbol,
                        client_order_id,
                        order_id,
                    } => {
                        tokio::spawn(async move {
                            if !symbol_matches(&event_symbol, &symbol) {
                                return;
                            }

                            let mut state = bot_state.write().await;
                            let removed = state.remove_pending_order(
                                client_order_id.as_deref(),
                                order_id.as_deref(),
                            );
                            let is_active_order = state
                                .active_order
                                .as_ref()
                                .map(|o| {
                                    client_order_id
                                        .as_ref()
                                        .map(|id| &o.client_order_id == id)
                                        .unwrap_or(false)
                                        || order_id
                                            .as_ref()
                                            .map(|id| o.exchange_order_id.as_deref() == Some(id.as_str()))
                                            .unwrap_or(false)
                                })
                                .unwrap_or(false);
                            if removed.is_some() && is_active_order && matches!(state.status, BotStatus::OrderPlaced) {
                                state.clear_active_order();
                                debug!("[BINANCE_FILL] Active order cancelled via stream");
                            }
                        });
                    }
                }
            })
            .await;

        run_result.map_err(|e| {
            error!(
                "{} {} User stream stopped: {}",
                "[BINANCE_FILL]".bright_magenta().bold(),
                "✗".red().bold(),
                e
            );
            e
        })
    }
}

fn symbol_matches(event_symbol: &str, target_symbol: &str) -> bool {
    if event_symbol.eq_ignore_ascii_case(target_symbol) {
        return true;
    }

    let target = target_symbol.to_ascii_uppercase();
    let variants = [
        format!("{target}USDT"),
        format!("{target}USDC"),
        format!("{target}BUSD"),
        format!("{target}FDUSD"),
    ];
    variants
        .iter()
        .any(|candidate| event_symbol.eq_ignore_ascii_case(candidate))
}
