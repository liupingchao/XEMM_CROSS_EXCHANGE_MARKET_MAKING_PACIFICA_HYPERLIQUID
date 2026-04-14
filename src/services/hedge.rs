use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use colored::Colorize;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream};
use fast_float::parse;
use tracing::{info, warn, error};

use crate::bot::BotState;
use crate::config::Config;
use crate::connector::hyperliquid::HyperliquidTrading;
use crate::connector::hyperliquid::types::{WsPostRequest, WsPostRequestInner, WsPostResponse};
use crate::connector::maker::MakerExchange;
use crate::services::HedgeEvent;
use crate::strategy::OrderSide;
use crate::trade_fetcher;
use crate::csv_logger;
use crate::audit_logger;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsWrite = futures_util::stream::SplitSink<WsStream, Message>;
type WsRead = futures_util::stream::SplitStream<WsStream>;



/// Hedge execution service
///
/// Receives hedge triggers via mpsc channel and executes the hedge flow:
/// 1. Pre-hedge cancellation of all maker orders
/// 2. Execute market order on Hyperliquid (opposite direction)
/// 3. Wait for trade propagation (20s)
/// 4. Fetch trade history from both exchanges
/// 5. Calculate actual profit using real fill data and fees
/// 6. Display comprehensive trade summary
/// 7. Post-hedge cancellation (safety)
/// 8. Position verification on maker + hedge exchanges
/// 9. Mark cycle complete and signal shutdown
pub struct HedgeService {
    pub bot_state: Arc<RwLock<BotState>>,
    pub hedge_rx: mpsc::UnboundedReceiver<HedgeEvent>,
    pub hyperliquid_prices: Arc<Mutex<(f64, f64)>>,
    pub config: Config,
    pub maker_symbol: String,
    pub hedge_symbol: String,
    pub hyperliquid_trading: Arc<HyperliquidTrading>,
    pub maker_exchange: Arc<dyn MakerExchange>,
    pub shutdown_tx: mpsc::Sender<()>,
    pub hl_wallet: String,
}

impl HedgeService {
    pub async fn run(mut self) {
        let use_ws_for_hedge = self.config.hyperliquid_use_ws_for_hedge;
        let mut ws_write: Option<WsWrite> = None;
        let mut ws_read: Option<WsRead> = None;
        let mut ws_request_id: u64 = 0;

        // Optionally establish trading WebSocket up front so it is hot
        if use_ws_for_hedge {
            match self.connect_hyperliquid_ws().await {
                Ok((write, read)) => {
                    info!(
                        "{} {} Hyperliquid trading WebSocket connected (hedge execution via WS)",
                        format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                        "✓".green().bold(),
                    );
                    ws_write = Some(write);
                    ws_read = Some(read);
                }
                Err(e) => {
                    warn!(
                        "{} {} Failed to pre-connect Hyperliquid trading WebSocket (using REST until reconnect succeeds): {}",
                        format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                        "⚠".yellow().bold(),
                        e
                    );
                }
            }
        }

        // Keep-alive interval for WebSocket pings (5s to keep connection warm)
        let mut keepalive_interval = tokio::time::interval(std::time::Duration::from_secs(5));
        keepalive_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                // Send periodic pings to keep WebSocket connection warm
                _ = keepalive_interval.tick() => {
                    if let Some(write) = ws_write.as_mut() {
                        if let Err(e) = write.send(Message::Ping(vec![])).await {
                            warn!("{} {} Failed to send keepalive ping: {}",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "⚠".yellow().bold(),
                                e
                            );
                            // Connection likely dead, clear write handle
                            ws_write = None;
                        }
                    }
                }

                // Main hedge event processing
                Some((side, size, avg_price, fill_timestamp)) = self.hedge_rx.recv() => {
            let reception_latency = fill_timestamp.elapsed();
            info!("{} ⚡ HEDGE RECEIVED: {} {} @ {} | Reception latency: {:.1}ms",
                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                side.as_str().bright_yellow(),
                size,
                format!("${:.4}", avg_price).cyan(),
                reception_latency.as_secs_f64() * 1000.0
            );

            // *** CRITICAL: CANCEL ALL ORDERS BEFORE HEDGE ***
            // Extra safety: cancel again in case fill detection missed anything
            // or there was a race condition.
            // MOVED TO BACKGROUND TASK to avoid blocking hedge execution latency.
            let maker_exchange_bg = self.maker_exchange.clone();
            let symbol_bg = self.maker_symbol.clone();

            tokio::spawn(async move {
                info!("{} {} Pre-hedge safety: Cancelling maker orders (background)...",
                    format!("[{} HEDGE]", symbol_bg).bright_magenta().bold(),
                    "⚡".yellow().bold()
                );

                if let Err(e) = maker_exchange_bg.cancel_all_orders(Some(&symbol_bg)).await
                {
                    warn!("{} {} Failed to cancel orders before hedge: {}",
                        format!("[{} HEDGE]", symbol_bg).bright_magenta().bold(),
                        "⚠".yellow().bold(),
                        e
                    );
                } else {
                    info!("{} {} Pre-hedge cancellation complete",
                        format!("[{} HEDGE]", symbol_bg).bright_magenta().bold(),
                        "✓".green().bold()
                    );
                }
            });

            // Update status
            {
                let mut state = self.bot_state.write().await;
                state.mark_hedging();
            }

            // Execute opposite direction on Hyperliquid
            let is_buy = match side {
                OrderSide::Buy => false, // Filled buy on Pacifica → sell on Hyperliquid
                OrderSide::Sell => true, // Filled sell on Pacifica → buy on Hyperliquid
            };

            let (mut hl_bid, mut hl_ask) = *self.hyperliquid_prices.lock();

            if hl_bid <= 0.0 || hl_ask <= 0.0 {
                warn!("{} {} Hyperliquid price cache empty - fetching fresh snapshot before hedging",
                    format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                    "⚠".yellow().bold()
                );

                const MAX_ATTEMPTS: usize = 5;
                for attempt in 1..=MAX_ATTEMPTS {
                    match self.hyperliquid_trading.get_l2_snapshot(&self.hedge_symbol).await {
                        Ok(Some((bid, ask))) if bid > 0.0 && ask > 0.0 => {
                            hl_bid = bid;
                            hl_ask = ask;
                            let mut cache = self.hyperliquid_prices.lock();
                            *cache = (bid, ask);
                            info!("{} {} Refreshed Hyperliquid prices: bid ${:.4}, ask ${:.4}",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "✓".green().bold(),
                                hl_bid,
                                hl_ask
                            );
                            break;
                        }
                        Ok(_) => {
                            warn!("{} {} Snapshot missing bid/ask data (attempt {}/{})",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "⚠".yellow().bold(),
                                attempt,
                                MAX_ATTEMPTS
                            );
                        }
                        Err(err) => {
                            warn!("{} {} Failed to fetch Hyperliquid snapshot (attempt {}/{}): {}",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "⚠".yellow().bold(),
                                attempt,
                                MAX_ATTEMPTS,
                                err
                            );
                        }
                    }

                    if attempt < MAX_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        let cached = *self.hyperliquid_prices.lock();
                        hl_bid = cached.0;
                        hl_ask = cached.1;
                        if hl_bid > 0.0 && hl_ask > 0.0 {
                            info!("{} {} Hyperliquid prices populated by feed during wait",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "✓".green().bold()
                            );
                            break;
                        }
                    }
                }

                if hl_bid <= 0.0 || hl_ask <= 0.0 {
                    error!("{} {} Unable to obtain Hyperliquid prices - aborting hedge for safety",
                        format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                        "✗".red().bold()
                    );

                    let mut state = self.bot_state.write().await;
                    state.set_error("Hyperliquid prices unavailable for hedge".to_string());

                    self.shutdown_tx.send(()).await.ok();
                    return;
                }
            }

            info!(
                "{} Executing {} {} on Hyperliquid",
                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                if is_buy { "BUY".green().bold() } else { "SELL".red().bold() },
                size
            );

            let hedge_result = if use_ws_for_hedge {
                // Ensure we have an active trading WebSocket
                if ws_write.is_none() || ws_read.is_none() {
                    match self.connect_hyperliquid_ws().await {
                        Ok((write, read)) => {
                            info!(
                                "{} {} Reconnected Hyperliquid trading WebSocket for hedge execution",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "✓".green().bold()
                            );
                            ws_write = Some(write);
                            ws_read = Some(read);
                        }
                        Err(e) => {
                            warn!(
                                "{} {} Failed to connect Hyperliquid trading WebSocket, falling back to REST for this hedge: {}",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "⚠".yellow().bold(),
                                e
                            );
                        }
                    }
                }

                if let (Some(write), Some(read)) = (ws_write.as_mut(), ws_read.as_mut()) {
                    match self
                        .place_market_order_ws(write, read, &mut ws_request_id, is_buy, size, hl_bid, hl_ask)
                        .await
                    {
                        Ok(response) => Ok(response),
                        Err(e) => {
                            warn!(
                                "{} {} WebSocket hedge execution failed, falling back to REST: {}",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "⚠".yellow().bold(),
                                e
                            );
                            // Drop WS so next hedge attempts a clean reconnect
                            ws_write = None;
                            ws_read = None;

                            self.hyperliquid_trading
                                .place_market_order(
                                    &self.hedge_symbol,
                                    is_buy,
                                    size,
                                    self.config.hyperliquid_slippage,
                                    false, // reduce_only
                                    Some(hl_bid),
                                    Some(hl_ask),
                                )
                                .await
                        }
                    }
                } else {
                    // No WS connection available – use REST for this hedge
                    self.hyperliquid_trading
                        .place_market_order(
                            &self.hedge_symbol,
                            is_buy,
                            size,
                            self.config.hyperliquid_slippage,
                            false, // reduce_only
                            Some(hl_bid),
                            Some(hl_ask),
                        )
                        .await
                }
            } else {
                // WS disabled via config – use REST only
                self.hyperliquid_trading
                    .place_market_order(
                        &self.hedge_symbol,
                        is_buy,
                        size,
                        self.config.hyperliquid_slippage,
                        false, // reduce_only
                        Some(hl_bid),
                        Some(hl_ask),
                    )
                    .await
            };

            match hedge_result {
                Ok(response) => {
                    // Extract success data from response
                    let response_data = match &response.response {
                        crate::connector::hyperliquid::OrderResponseContent::Success(data) => data,
                        crate::connector::hyperliquid::OrderResponseContent::Error(error) => {
                            // This should not happen as trading.rs already handles errors,
                            // but handle it defensively
                            error!("{} {} Hedge response contains error: {}",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "✗".red().bold(),
                                error
                            );

                            let mut state = self.bot_state.write().await;
                            state.set_error(format!("Hedge failed: {}", error));
                            drop(state);

                            self.shutdown_tx.send(()).await.ok();
                            return;
                        }
                    };

                    // Calculate ACTUAL end-to-end latency from fill detection to hedge completion
                    let end_to_end_latency = fill_timestamp.elapsed();
                    let hedge_side = if is_buy { OrderSide::Buy } else { OrderSide::Sell };

                    // Validate and extract order status
                    let hedge_fill_result = if let Some(status) = response_data.data.statuses.first() {
                        match status {
                            crate::connector::hyperliquid::OrderStatus::Filled { filled } => {
                                let parsed_price = filled.avgPx.parse::<f64>().ok();
                                if parsed_price.is_none() {
                                    warn!("{} {} Failed to parse hedge avgPx '{}' - fill price unknown",
                                        format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                        "⚠".yellow().bold(),
                                        filled.avgPx
                                    );
                                }
                                let parsed_size = match filled.totalSz.parse::<f64>() {
                                    Ok(v) if v > 0.0 => v,
                                    Ok(v) => {
                                        warn!("{} {} Hedge totalSz parsed as non-positive ({}) - falling back to requested size {}",
                                            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                            "⚠".yellow().bold(),
                                            v, size
                                        );
                                        size
                                    }
                                    Err(e) => {
                                        warn!("{} {} Failed to parse hedge totalSz '{}': {} - falling back to requested size {}",
                                            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                            "⚠".yellow().bold(),
                                            filled.totalSz, e, size
                                        );
                                        size
                                    }
                                };
                                info!("{} {} Hedge executed successfully: Filled {} @ ${} | Total latency: {:.1}ms",
                                    format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                    "✓".green().bold(),
                                    filled.totalSz,
                                    filled.avgPx,
                                    end_to_end_latency.as_secs_f64() * 1000.0
                                );
                                parsed_price.map(|price| {
                                    (
                                        price,
                                        parsed_size,
                                        Some(filled.oid.to_string()),
                                    )
                                })
                            }
                            crate::connector::hyperliquid::OrderStatus::Error { error } => {
                                error!("{} {} Hedge order FAILED: {}",
                                    format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                    "✗".red().bold(),
                                    error
                                );

                                // Set error state
                                {
                                    let mut state = self.bot_state.write().await;
                                    state.set_error(format!("Hedge order failed: {}", error));
                                }

                                // Signal shutdown with error
                                self.shutdown_tx.send(()).await.ok();
                                return;  // Exit hedge service immediately
                            }
                            crate::connector::hyperliquid::OrderStatus::Resting { resting } => {
                                warn!("{} {} Hedge order is RESTING (oid: {}) - unexpected for IOC market order",
                                    format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                    "⚠".yellow().bold(),
                                    resting.oid
                                );

                                // Treat as error - IOC orders should never rest
                                {
                                    let mut state = self.bot_state.write().await;
                                    state.set_error(format!("Hedge order resting (unexpected for IOC): oid {}", resting.oid));
                                }

                                self.shutdown_tx.send(()).await.ok();
                                return;
                            }
                        }
                    } else {
                        warn!("{} {} Hedge response has no statuses - unexpected API response",
                            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                            "⚠".yellow().bold()
                        );
                        None
                    };

                    // Validate we got a fill price before continuing
                    let (hedge_fill_price, hedge_fill_size, hedge_order_id) = match hedge_fill_result {
                        Some(fill_data) => fill_data,
                        None => {
                            error!("{} {} No hedge fill price available - hedge may have failed",
                                format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                                "✗".red().bold()
                            );

                            {
                                let mut state = self.bot_state.write().await;
                                state.set_error("Hedge execution failed - no fill price".to_string());
                            }

                            self.shutdown_tx.send(()).await.ok();
                            return;
                        }
                    };
                    // Use relative tolerance: fill is "full" if >= 99.99% of requested size
                    let hedge_is_full_fill = size <= 0.0 || (hedge_fill_size / size) >= 0.9999;

                    // Audit logging for hedge side (Hyperliquid): order + fill
                    let hedge_orders_csv = audit_logger::order_file_for_symbol(&self.hedge_symbol);
                    let hedge_order_record = audit_logger::OrderRecord::new(
                        chrono::Utc::now(),
                        self.hedge_symbol.clone(),
                        "Hyperliquid".to_string(),
                        hedge_side,
                        hedge_fill_size,
                        hedge_fill_price,
                        hedge_order_id.clone(),
                        None,
                        "hedge_market_order".to_string(),
                    );
                    if let Err(e) = audit_logger::log_order(&hedge_orders_csv, &hedge_order_record) {
                        warn!(
                            "{} {} Failed to log hedge order to CSV: {}",
                            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                            "⚠".yellow().bold(),
                            e
                        );
                    }

                    let hedge_fills_csv = audit_logger::fill_file_for_symbol(&self.hedge_symbol);
                    let hedge_fill_record = audit_logger::FillRecord::new(
                        chrono::Utc::now(),
                        self.hedge_symbol.clone(),
                        "Hyperliquid".to_string(),
                        hedge_side,
                        hedge_fill_size,
                        hedge_fill_price,
                        hedge_is_full_fill,
                        None,
                        hedge_order_id.clone(),
                        "hedge_market_fill".to_string(),
                    );
                    if let Err(e) = audit_logger::log_fill(&hedge_fills_csv, &hedge_fill_record) {
                        warn!(
                            "{} {} Failed to log hedge fill to CSV: {}",
                            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                            "⚠".yellow().bold(),
                            e
                        );
                    }
                    if !hedge_is_full_fill {
                        warn!(
                            "{} {} Hedge fill appears partial: expected {:.6}, actual {:.6}",
                            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                            "⚠".yellow().bold(),
                            size,
                            hedge_fill_size
                        );
                    }

                    // Snapshot data needed for post-hedge audit before resetting state
                    let expected_profit_bps = {
                        let state = self.bot_state.read().await;
                        state.active_order.as_ref().map(|o| o.initial_profit_bps)
                    };
                    let (snapshot_client_order_id, snapshot_maker_order_id) = {
                        let state = self.bot_state.read().await;
                        (
                            state.active_order.as_ref().map(|o| o.client_order_id.clone()),
                            state.active_order.as_ref().and_then(|o| o.exchange_order_id.clone()),
                        )
                    };

                    // In continuous mode: return to Idle IMMEDIATELY so the next
                    // cycle can start while post-hedge audit runs in background.
                    // In single-shot mode: keep synchronous behavior (bot exits anyway).
                    if self.config.continuous_mode {
                        let mut state = self.bot_state.write().await;
                        state.mark_cycle_complete_continue();
                        drop(state);
                        info!(
                            "{} {} Cycle completed, returning to Idle (continuous mode, audit runs in background)",
                            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                            "✓".green().bold()
                        );

                        // Spawn background task for post-hedge audit & verification
                        let bg_config = self.config.clone();
                        let bg_maker_symbol = self.maker_symbol.clone();
                        let bg_hedge_symbol = self.hedge_symbol.clone();
                        let bg_hl_wallet = self.hl_wallet.clone();
                        let bg_maker_exchange = self.maker_exchange.clone();
                        let bg_hl_trading = self.hyperliquid_trading.clone();

                        tokio::spawn(async move {
                            Self::post_hedge_audit(
                                bg_config,
                                bg_maker_symbol,
                                bg_hedge_symbol,
                                bg_hl_wallet,
                                bg_maker_exchange,
                                bg_hl_trading,
                                side,
                                is_buy,
                                size,
                                avg_price,
                                hedge_fill_price,
                                end_to_end_latency,
                                expected_profit_bps,
                                snapshot_client_order_id,
                                snapshot_maker_order_id,
                            ).await;
                        });
                    } else {
                        // Single-shot mode: run audit synchronously then shutdown
                        Self::post_hedge_audit(
                            self.config.clone(),
                            self.maker_symbol.clone(),
                            self.hedge_symbol.clone(),
                            self.hl_wallet.clone(),
                            self.maker_exchange.clone(),
                            self.hyperliquid_trading.clone(),
                            side,
                            is_buy,
                            size,
                            avg_price,
                            hedge_fill_price,
                            end_to_end_latency,
                            expected_profit_bps,
                            snapshot_client_order_id,
                            snapshot_maker_order_id,
                        ).await;

                        let mut state = self.bot_state.write().await;
                        state.mark_complete();
                        drop(state);
                        self.shutdown_tx.send(()).await.ok();
                    }
                }
                Err(e) => {
                    error!("{} {} Hedge failed: {}",
                        format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                        "✗".red().bold(),
                        e.to_string().red()
                    );

                    // *** CRITICAL: CANCEL ALL ORDERS ON ERROR ***
                    // Even if hedge fails, cancel all orders to prevent stray positions
                    info!("{} {} Error recovery: Cancelling all maker orders...",
                        format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                        "⚡".yellow().bold()
                    );

                    if let Err(cancel_err) = self
                        .maker_exchange
                        .cancel_all_orders(Some(&self.maker_symbol))
                        .await
                    {
                        warn!("{} {} Failed to cancel orders after hedge error: {}",
                            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                            "⚠".yellow().bold(),
                            cancel_err
                        );
                    } else {
                        info!("{} {} Error recovery cancellation complete",
                            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
                            "✓".green().bold()
                        );
                    }

                    let mut state = self.bot_state.write().await;
                    state.set_error(format!("Hedge failed: {}", e));

                    // Signal shutdown with error
                    self.shutdown_tx.send(()).await.ok();
                }
            }
                } // Close Some((side, size, avg_price, fill_timestamp)) arm
            } // Close tokio::select!
        } // Close loop
    }

    /// Post-hedge audit: fetch trade history, calculate actual profit, log CSV,
    /// cancel remaining orders, and verify positions.
    ///
    /// In continuous mode this runs as a background task so the next cycle can
    /// start immediately after a successful hedge.
    async fn post_hedge_audit(
        config: Config,
        maker_symbol: String,
        hedge_symbol: String,
        hl_wallet: String,
        maker_exchange: Arc<dyn MakerExchange>,
        hyperliquid_trading: Arc<HyperliquidTrading>,
        side: OrderSide,
        is_buy: bool,
        size: f64,
        avg_price: f64,
        hedge_fill_price: f64,
        end_to_end_latency: Duration,
        expected_profit_bps: Option<f64>,
        client_order_id: Option<String>,
        maker_order_id: Option<String>,
    ) {
        let symbol_tag = config.symbol.clone();

        // Wait for trades to propagate to exchange APIs
        info!("{} Waiting 20 seconds for trades to propagate to APIs...",
            format!("[{} PROFIT]", symbol_tag).bright_blue().bold()
        );
        tokio::time::sleep(Duration::from_secs(20)).await;

        // Fetch maker trade history with retry logic
        let (maker_fill_price, maker_actual_fee, maker_notional): (Option<f64>, Option<f64>, Option<f64>) =
            if client_order_id.is_some() || maker_order_id.is_some() {
                let symbol_tag2 = symbol_tag.clone();
                let result = trade_fetcher::fetch_maker_trade(
                    maker_exchange.clone(),
                    &maker_symbol,
                    client_order_id.as_deref(),
                    maker_order_id.as_deref(),
                    3,
                    move |msg| {
                        info!("{} {}",
                            format!("[{} PROFIT]", symbol_tag2).bright_blue().bold(),
                            msg
                        );
                    },
                )
                .await;
                (result.fill_price, result.actual_fee, result.total_notional)
            } else {
                (None, None, None)
            };

        // Fetch Hyperliquid user fills with retry logic
        let symbol_tag3 = symbol_tag.clone();
        let (hl_fill_price, hl_actual_fee, hl_notional): (Option<f64>, Option<f64>, Option<f64>) = {
            let result = trade_fetcher::fetch_hyperliquid_fills(
                &hyperliquid_trading,
                &hl_wallet,
                &hedge_symbol,
                3,
                30,
                move |msg| {
                    info!("{} {}",
                        format!("[{} PROFIT]", symbol_tag3).bright_blue().bold(),
                        msg
                    );
                }
            ).await;
            (result.fill_price, result.actual_fee, result.total_notional)
        };

        // Calculate actual profitability
        let (actual_profit_bps, actual_profit_usd, maker_actual_price, hl_actual_price, maker_fee_usd, hl_fee_usd) =
            match (maker_notional, hl_notional, maker_fill_price, hl_fill_price) {
                (Some(pac_notional), Some(hl_notional), pac_price_opt, hl_price_opt) => {
                    let pac_fee = maker_actual_fee.unwrap_or_else(|| {
                        pac_notional * (config.pacifica_maker_fee_bps / 10000.0)
                    });
                    let hl_fee = hl_actual_fee.unwrap_or_else(|| {
                        hl_notional * (config.hyperliquid_taker_fee_bps / 10000.0)
                    });
                    let is_pacifica_buy = matches!(side, OrderSide::Buy);
                    let profit = trade_fetcher::calculate_hedge_profit(
                        pac_notional,
                        hl_notional,
                        pac_fee,
                        hl_fee,
                        is_pacifica_buy,
                    );
                    (profit.profit_bps, profit.net_profit, pac_price_opt, hl_price_opt, pac_fee, hl_fee)
                }
                _ => {
                    warn!("{} {} Using fill event data (trade history unavailable)",
                        format!("[{} PROFIT]", symbol_tag).bright_blue().bold(),
                        "⚠".yellow().bold()
                    );
                    let hl_price = hedge_fill_price;
                    let pac_price = avg_price;
                    let pac_fee = pac_price * size * (config.pacifica_maker_fee_bps / 10000.0);
                    let hl_fee = hl_price * size * (config.hyperliquid_taker_fee_bps / 10000.0);
                    let (profit_usd, cost, _revenue) = match side {
                        OrderSide::Buy => {
                            let cost = (pac_price * size) + pac_fee;
                            let revenue = (hl_price * size) - hl_fee;
                            (revenue - cost, cost, revenue)
                        }
                        OrderSide::Sell => {
                            let revenue = (pac_price * size) - pac_fee;
                            let cost = (hl_price * size) + hl_fee;
                            (revenue - cost, cost, revenue)
                        }
                    };
                    let profit_rate = if cost > 0.0 { profit_usd / cost } else { 0.0 };
                    let profit_bps = profit_rate * 10000.0;
                    (profit_bps, profit_usd, Some(pac_price), Some(hl_price), pac_fee, hl_fee)
                }
            };

        // Log trade to CSV
        if maker_actual_price.is_some() && hl_actual_price.is_some() {
            let trade_record = csv_logger::TradeRecord::new(
                Utc::now(),
                end_to_end_latency.as_secs_f64() * 1000.0,
                maker_symbol.clone(),
                side,
                maker_actual_price.unwrap(),
                size,
                maker_notional.unwrap_or(maker_actual_price.unwrap() * size),
                maker_fee_usd,
                hl_actual_price.unwrap(),
                size,
                hl_notional.unwrap_or(hl_actual_price.unwrap() * size),
                hl_fee_usd,
                expected_profit_bps.unwrap_or(0.0),
                actual_profit_bps,
                actual_profit_usd,
            );
            let csv_file = format!("{}_trades.csv", maker_symbol.to_lowercase());
            if let Err(e) = csv_logger::log_trade(&csv_file, &trade_record) {
                warn!("{} {} Failed to log trade to CSV: {}",
                    format!("[{} CSV]", symbol_tag).bright_yellow().bold(),
                    "⚠".yellow().bold(),
                    e
                );
            } else {
                info!("{} {} Trade logged to {}",
                    format!("[{} CSV]", symbol_tag).bright_green().bold(),
                    "✓".green().bold(),
                    csv_file
                );
            }
        }

        // Display comprehensive summary
        info!("{}", "═══════════════════════════════════════════════════".green().bold());
        info!("{}", "  CYCLE AUDIT COMPLETE".green().bold());
        info!("{}", "═══════════════════════════════════════════════════".green().bold());
        info!("");
        info!("{}", "TRADE SUMMARY:".bright_white().bold());
        if let Some(maker_price) = maker_actual_price {
            info!("  {}: {} {} {} @ {} {}",
                maker_exchange.name().bright_magenta(),
                side.as_str().bright_yellow(),
                format!("{:.4}", size).bright_white(),
                maker_symbol.bright_white().bold(),
                format!("${:.6}", maker_price).cyan().bold(),
                "(actual fill)".bright_black()
            );
        }
        if let Some(hl_price) = hl_actual_price {
            info!("  {}: {} {} {} @ {} {}",
                "Hyperliquid".bright_magenta(),
                if is_buy { "BUY".green() } else { "SELL".red() },
                format!("{:.4}", size).bright_white(),
                hedge_symbol.bright_white().bold(),
                format!("${:.6}", hl_price).cyan().bold(),
                "(actual fill)".bright_black()
            );
        }
        info!("");
        info!("{}", "PROFITABILITY:".bright_white().bold());
        if let Some(expected) = expected_profit_bps {
            info!("  Expected: {} bps", format!("{:.2}", expected).bright_white());
        }
        if maker_actual_price.is_some() && hl_actual_price.is_some() {
            let profit_color = if actual_profit_bps > 0.0 { format!("{:.2}", actual_profit_bps).green().bold() } else { format!("{:.2}", actual_profit_bps).red().bold() };
            let usd_color = if actual_profit_usd > 0.0 { format!("${:.4}", actual_profit_usd).green().bold() } else { format!("${:.4}", actual_profit_usd).red().bold() };
            info!("  Actual:   {} bps ({})", profit_color, usd_color);
            if let Some(expected) = expected_profit_bps {
                let diff = actual_profit_bps - expected;
                let diff_sign = if diff >= 0.0 { "+" } else { "" };
                let diff_color = if diff >= 0.0 { format!("{}{:.2}", diff_sign, diff).green() } else { format!("{:.2}", diff).red() };
                info!("  Difference: {} bps", diff_color);
            }
        } else {
            warn!("  {} Unable to calculate actual profit (trade history unavailable)", "⚠".yellow().bold());
        }
        info!("");
        info!("{}", "FEES:".bright_white().bold());
        if maker_actual_price.is_some() && hl_actual_price.is_some() {
            info!("  {}: {} {}",
                maker_exchange.name(),
                format!("${:.4}", maker_fee_usd).yellow(),
                if maker_actual_fee.is_some() { "(actual)" } else { "(estimated)" }.bright_black()
            );
            info!("  Hyperliquid: {} {}",
                format!("${:.4}", hl_fee_usd).yellow(),
                if hl_actual_fee.is_some() { "(actual)" } else { "(estimated)" }.bright_black()
            );
            info!("  Total: {}", format!("${:.4}", maker_fee_usd + hl_fee_usd).yellow().bold());
        } else {
            info!("  {} (maker): {} bps", maker_exchange.name(), format!("{:.2}", config.pacifica_maker_fee_bps).yellow());
            info!("  Hyperliquid (taker): {} bps", format!("{:.2}", config.hyperliquid_taker_fee_bps).yellow());
            info!("  Total fees: {} bps", format!("{:.2}", config.pacifica_maker_fee_bps + config.hyperliquid_taker_fee_bps).yellow().bold());
        }
        info!("{}", "═══════════════════════════════════════════════════".green().bold());

        // Safety cancellation
        info!("{} {} Post-hedge safety: Cancelling all maker orders...",
            format!("[{} AUDIT]", symbol_tag).bright_magenta().bold(),
            "⚡".yellow().bold()
        );
        if let Err(e) = maker_exchange.cancel_all_orders(Some(&maker_symbol)).await {
            warn!("{} {} Failed to cancel orders after hedge: {}",
                format!("[{} AUDIT]", symbol_tag).bright_magenta().bold(),
                "⚠".yellow().bold(),
                e
            );
        }

        // Position verification
        info!("{} Waiting 8 seconds for positions to propagate...",
            format!("[{} AUDIT]", symbol_tag).bright_magenta().bold()
        );
        tokio::time::sleep(Duration::from_secs(8)).await;

        let maker_position = match maker_exchange.get_positions(Some(&maker_symbol)).await {
            Ok(positions) => {
                if let Some(pos) = positions.first() {
                    let signed_amount = pos.signed_amount();
                    info!("{} {}: {:.4} {} (signed: {:.4})",
                        format!("[{} VERIFY]", symbol_tag).cyan().bold(),
                        maker_exchange.name(),
                        pos.amount, pos.side.as_str(), signed_amount
                    );
                    Some(signed_amount)
                } else {
                    Some(0.0)
                }
            }
            Err(e) => { warn!("[VERIFY] Failed to fetch maker position: {}", e); None }
        };

        let mut hyperliquid_position: Option<f64> = None;
        for retry in 0..3 {
            if retry > 0 {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            match hyperliquid_trading.get_user_state(&hl_wallet).await {
                Ok(user_state) => {
                    if let Some(asset_pos) = user_state.asset_positions.iter().find(|ap| ap.position.coin == hedge_symbol) {
                        let szi: f64 = parse(&asset_pos.position.szi).unwrap_or(0.0);
                        info!("{} Hyperliquid: {} (signed: {:.4})",
                            format!("[{} VERIFY]", symbol_tag).cyan().bold(),
                            if szi > 0.0 { "LONG".green() } else if szi < 0.0 { "SHORT".red() } else { "FLAT".bright_white() },
                            szi
                        );
                        hyperliquid_position = Some(szi);
                        break;
                    } else if retry == 2 {
                        hyperliquid_position = Some(0.0);
                    }
                }
                Err(e) => {
                    if retry == 2 {
                        warn!("[VERIFY] Failed to fetch Hyperliquid position after 3 attempts: {}", e);
                    }
                }
            }
        }

        if let (Some(maker_pos), Some(hl_pos)) = (maker_position, hyperliquid_position) {
            let net_position = maker_pos + hl_pos;
            info!("{} Net Position: {:.4} ({}: {:.4} + Hyperliquid: {:.4})",
                format!("[{} VERIFY]", symbol_tag).cyan().bold(),
                net_position, maker_exchange.name(), maker_pos, hl_pos
            );
            if net_position.abs() >= 0.01 {
                warn!("{}", "WARNING: Net position NOT neutral!".red().bold());
                warn!("Position delta: {:.4} {}|{}", net_position.abs(), maker_symbol, hedge_symbol);
                warn!("Check positions manually and rebalance if needed!");
            }
        } else {
            warn!("[VERIFY] Could not verify net position - check manually");
        }
    }

    /// Establish a Hyperliquid trading WebSocket connection for hedging.
    async fn connect_hyperliquid_ws(&self) -> anyhow::Result<(WsWrite, WsRead)> {
        let ws_url = if self.hyperliquid_trading.is_testnet() {
            "wss://api.hyperliquid-testnet.xyz/ws"
        } else {
            "wss://api.hyperliquid.xyz/ws"
        };

        let (ws_stream, _) = connect_async(ws_url).await?;
        let (write, read) = ws_stream.split();
        Ok((write, read))
    }

    /// Place a market IOC order over Hyperliquid WebSocket using the shared
    /// REST signing and request-building logic.
    async fn place_market_order_ws(
        &self,
        write: &mut WsWrite,
        read: &mut WsRead,
        request_id_counter: &mut u64,
        is_buy: bool,
        size: f64,
        bid: f64,
        ask: f64,
    ) -> anyhow::Result<crate::connector::hyperliquid::OrderResponse> {
        // Build signed order payload (same as REST)
        let payload = self
            .hyperliquid_trading
            .build_market_order_request(
                &self.hedge_symbol,
                is_buy,
                size,
                self.config.hyperliquid_slippage,
                false,
                Some(bid),
                Some(ask),
            )
            .await?;

        *request_id_counter += 1;
        let request_id = *request_id_counter;

        let ws_request = WsPostRequest {
            method: "post".to_string(),
            id: request_id,
            request: WsPostRequestInner {
                type_: "action".to_string(),
                payload,
            },
        };

        let request_json = serde_json::to_string(&ws_request)?;
        info!(
            "{} Sending Hyperliquid hedge order via WebSocket (id={})",
            format!("[{} HEDGE]", self.config.symbol).bright_magenta().bold(),
            request_id
        );
        write.send(Message::Text(request_json)).await?;

        // Wait for the matching post response
        loop {
            match read.next().await {
                Some(Ok(Message::Text(text))) => {
                    // Try to parse as a generic post response
                    let ws_resp: WsPostResponse = match serde_json::from_str(&text) {
                        Ok(r) => r,
                        Err(_) => {
                            // Ignore unrelated/non-standard messages
                            continue;
                        }
                    };

                    if ws_resp.channel != "post" || ws_resp.data.id != request_id {
                        // Response for another request or channel – ignore
                        continue;
                    }

                    let resp_type = ws_resp.data.response.type_;
                    let payload = ws_resp.data.response.payload;

                    return match resp_type.as_str() {
                        "action" => {
                            let order_response: crate::connector::hyperliquid::OrderResponse =
                                serde_json::from_value(payload)?;
                            Ok(order_response)
                        }
                        "error" => {
                            let msg = payload
                                .as_str()
                                .unwrap_or("Unknown Hyperliquid WebSocket error")
                                .to_string();
                            anyhow::bail!("Hyperliquid WebSocket order error: {}", msg);
                        }
                        other => {
                            anyhow::bail!("Unexpected Hyperliquid WebSocket response type: {}", other);
                        }
                    };
                }
                Some(Ok(Message::Ping(data))) => {
                    // Respond to low-level WebSocket ping frames
                    write.send(Message::Pong(data)).await?;
                }
                Some(Ok(Message::Pong(_))) => {
                    // Ignore
                }
                Some(Ok(Message::Close(frame))) => {
                    anyhow::bail!("Hyperliquid WebSocket closed: {:?}", frame);
                }
                Some(Err(e)) => {
                    anyhow::bail!("Hyperliquid WebSocket error: {}", e);
                }
                None => {
                    anyhow::bail!("Hyperliquid WebSocket stream ended unexpectedly");
                }
                _ => {}
            }
        }
    }
}
