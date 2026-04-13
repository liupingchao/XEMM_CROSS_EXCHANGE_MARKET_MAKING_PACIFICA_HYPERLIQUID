use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;
use xemm_rust::connector::binance::{BinanceCredentials, BinanceTrading};
use xemm_rust::connector::hyperliquid::{HyperliquidCredentials, HyperliquidTrading};
use xemm_rust::connector::maker::MakerOrderSide;

fn parse_env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(default)
}

fn parse_env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "y"))
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    dotenv::dotenv().ok();

    let binance_symbol = std::env::var("SMOKE_BINANCE_SYMBOL").unwrap_or_else(|_| "CLUSDT".to_string());
    let hl_symbol = std::env::var("SMOKE_HL_SYMBOL").unwrap_or_else(|_| "WTIOIL".to_string());
    let notional_usd = parse_env_f64("SMOKE_NOTIONAL_USD", 10.0);
    let execute = parse_env_bool("SMOKE_EXECUTE", false);

    info!("=== Smoke Test Config ===");
    info!("Binance symbol: {}", binance_symbol);
    info!("Hyperliquid symbol: {}", hl_symbol);
    info!("Notional: ${:.2}", notional_usd);
    info!("Execute trades: {}", execute);

    let binance_creds = BinanceCredentials::from_env().context("Failed to load Binance creds")?;
    let hl_creds = HyperliquidCredentials::from_env().context("Failed to load Hyperliquid creds")?;

    let binance = BinanceTrading::new(binance_creds, false)?;
    let hl = HyperliquidTrading::new(hl_creds, false)?;

    // Read-only connectivity checks
    let binance_info = binance.get_symbol_info(&binance_symbol).await?;
    let binance_tob = binance
        .get_best_bid_ask_rest(&binance_symbol)
        .await?
        .context("No Binance TOB")?;
    info!(
        "[BINANCE] tick_size={}, lot_size={}, bid={:.6}, ask={:.6}",
        binance_info.tick_size, binance_info.lot_size, binance_tob.0, binance_tob.1
    );

    let hl_tob = hl
        .get_l2_snapshot(&hl_symbol)
        .await?
        .context("No Hyperliquid TOB")?;
    info!("[HYPERLIQUID] bid={:.6}, ask={:.6}", hl_tob.0, hl_tob.1);

    if !execute {
        info!("Read-only connectivity test finished. Set SMOKE_EXECUTE=true to place trades.");
        return Ok(());
    }

    // Binance: open + close with ~10 USDC notional
    let binance_qty = notional_usd / binance_tob.1.max(1e-12);
    info!(
        "[BINANCE] MARKET BUY then SELL, raw qty={:.8} (~${:.2})",
        binance_qty, notional_usd
    );
    let b_buy = binance
        .place_market_order(&binance_symbol, MakerOrderSide::Buy, binance_qty)
        .await?;
    info!(
        "[BINANCE] BUY sent: order_id={:?}, cloid={:?}",
        b_buy.order_id, b_buy.client_order_id
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let b_sell = binance
        .place_market_order(&binance_symbol, MakerOrderSide::Sell, binance_qty)
        .await?;
    info!(
        "[BINANCE] SELL sent: order_id={:?}, cloid={:?}",
        b_sell.order_id, b_sell.client_order_id
    );

    // Hyperliquid: open + close with ~10 USDC notional
    let hl_qty = notional_usd / hl_tob.1.max(1e-12);
    info!(
        "[HYPERLIQUID] MARKET BUY then SELL, raw qty={:.8} (~${:.2})",
        hl_qty, notional_usd
    );
    let h_buy = hl
        .place_market_order(
            &hl_symbol,
            true,
            hl_qty,
            0.03,
            false,
            Some(hl_tob.0),
            Some(hl_tob.1),
        )
        .await?;
    info!("[HYPERLIQUID] BUY response: {:?}", h_buy.response);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let hl_tob2 = hl
        .get_l2_snapshot(&hl_symbol)
        .await?
        .context("No Hyperliquid TOB for close")?;
    let h_sell = hl
        .place_market_order(
            &hl_symbol,
            false,
            hl_qty,
            0.03,
            false,
            Some(hl_tob2.0),
            Some(hl_tob2.1),
        )
        .await?;
    info!("[HYPERLIQUID] SELL response: {:?}", h_sell.response);

    info!("Smoke trade execution finished.");
    Ok(())
}
