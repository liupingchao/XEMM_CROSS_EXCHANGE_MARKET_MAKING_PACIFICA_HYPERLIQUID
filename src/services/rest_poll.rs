use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Duration;
use tokio::time::interval;
use tracing::debug;

use crate::connector::hyperliquid::HyperliquidTrading;
use crate::connector::maker::MakerExchange;

/// Maker REST API polling service
///
/// Complements WebSocket orderbook by polling REST API periodically.
/// Provides redundancy if WebSocket connection is lost or delayed.
/// Updates shared price state at configured interval.
pub struct MakerRestPollService {
    pub prices: Arc<Mutex<(f64, f64)>>,
    pub maker_exchange: Arc<dyn MakerExchange>,
    pub symbol: String,
    pub agg_level: u32,
    pub poll_interval_secs: u64,
}

impl MakerRestPollService {
    pub async fn run(self) {
        let mut interval_timer = interval(Duration::from_secs(self.poll_interval_secs));
        interval_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval_timer.tick().await;

            match self
                .maker_exchange
                .get_best_bid_ask_rest(&self.symbol, self.agg_level)
                .await
            {
                Ok(Some((bid, ask))) => {
                    // Update shared orderbook prices
                    *self.prices.lock() = (bid, ask);
                    debug!(
                        "[{}_REST] Updated prices via REST: bid=${:.4}, ask=${:.4}",
                        self.maker_exchange.name().to_uppercase(),
                        bid, ask
                    );
                }
                Ok(None) => {
                    debug!(
                        "[{}_REST] No bid/ask available from REST API",
                        self.maker_exchange.name().to_uppercase()
                    );
                }
                Err(e) => {
                    debug!(
                        "[{}_REST] Failed to fetch prices: {}",
                        self.maker_exchange.name().to_uppercase(),
                        e
                    );
                }
            }
        }
    }
}

/// Hyperliquid REST API polling service
///
/// Complements WebSocket orderbook by polling REST API periodically.
/// Provides redundancy if WebSocket connection is lost or delayed.
/// Updates shared price state at configured interval (typically 2s).
pub struct HyperliquidRestPollService {
    pub prices: Arc<Mutex<(f64, f64)>>,
    pub hyperliquid_trading: Arc<HyperliquidTrading>,
    pub symbol: String,
    pub poll_interval_secs: u64,
}

impl HyperliquidRestPollService {
    pub async fn run(self) {
        let mut interval_timer = interval(Duration::from_secs(self.poll_interval_secs));
        interval_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval_timer.tick().await;

            match self.hyperliquid_trading.get_l2_snapshot(&self.symbol).await {
                Ok(Some((bid, ask))) => {
                    // Update shared orderbook prices
                    *self.prices.lock() = (bid, ask);
                    debug!(
                        "[HYPERLIQUID_REST] Updated prices via REST: bid=${:.4}, ask=${:.4}",
                        bid, ask
                    );
                }
                Ok(None) => {
                    debug!("[HYPERLIQUID_REST] No bid/ask available from REST API");
                }
                Err(e) => {
                    debug!("[HYPERLIQUID_REST] Failed to fetch prices: {}", e);
                }
            }
        }
    }
}
