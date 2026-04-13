use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::time::interval;
use tracing::{info, warn};

use xemm_rust::config::Config;
use xemm_rust::connector::binance::{BinanceCredentials, BinanceTrading};
use xemm_rust::connector::hyperliquid::{HyperliquidCredentials, HyperliquidTrading};
use xemm_rust::connector::maker::{BinanceMakerExchange, MakerExchange, PacificaMakerExchange};
use xemm_rust::connector::pacifica::{PacificaCredentials, PacificaTrading};
use xemm_rust::services::spread_recorder::SpreadCsvRecorder;

fn parse_env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn parse_env_i64(key: &str, default: i64) -> i64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
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
    let config = Config::load_default().context("Failed to load config.json")?;

    let maker_symbol =
        env::var("SPREAD_TEST_MAKER_SYMBOL").unwrap_or_else(|_| config.symbol.clone());
    let hl_symbol = env::var("SPREAD_TEST_HL_SYMBOL").unwrap_or_else(|_| config.symbol.clone());
    let duration_secs = parse_env_u64("SPREAD_TEST_DURATION_SECS", 60);
    let sample_interval_secs = parse_env_u64("SPREAD_TEST_SAMPLE_INTERVAL_SECS", 1).max(1);
    let retention_hours = parse_env_i64("SPREAD_TEST_RETENTION_HOURS", 24);
    let file_path = env::var("SPREAD_TEST_FILE").unwrap_or_else(|_| {
        format!(
            "{}_{}_spread_history.csv",
            maker_symbol.to_lowercase(),
            hl_symbol.to_lowercase().replace(':', "_")
        )
    });

    let maker_exchange: Arc<dyn MakerExchange> = match config.maker_exchange.to_ascii_lowercase().as_str() {
        "pacifica" => {
            let creds = PacificaCredentials::from_env()?;
            let trading = Arc::new(PacificaTrading::new(creds)?);
            Arc::new(PacificaMakerExchange::new(trading))
        }
        "binance" => {
            let creds = BinanceCredentials::from_env()?;
            let trading = Arc::new(BinanceTrading::new(creds, false)?);
            Arc::new(BinanceMakerExchange::new(trading))
        }
        other => anyhow::bail!("Unsupported maker_exchange in config: {}", other),
    };

    let hl_creds = HyperliquidCredentials::from_env()?;
    let hl_trading = HyperliquidTrading::new(hl_creds, false)?;

    let mut recorder = SpreadCsvRecorder::new(
        file_path.clone(),
        format!("{}|{}", maker_symbol, hl_symbol),
        maker_exchange.name().to_string(),
        retention_hours,
    );
    recorder.load_existing()?;

    info!("=== Spread Record Test ===");
    info!("Maker exchange: {}", maker_exchange.name());
    info!("Maker symbol: {}", maker_symbol);
    info!("Hyperliquid symbol: {}", hl_symbol);
    info!("Duration: {}s", duration_secs);
    info!("Sample interval: {}s", sample_interval_secs);
    info!("Retention: {}h", retention_hours);
    info!("Output file: {}", file_path);

    let mut ticker = interval(Duration::from_secs(sample_interval_secs));
    let start = Instant::now();
    let mut samples = 0u64;

    while start.elapsed() < Duration::from_secs(duration_secs) {
        ticker.tick().await;

        let maker_tob = maker_exchange
            .get_best_bid_ask_rest(&maker_symbol, config.agg_level)
            .await?;
        let hl_tob = hl_trading.get_l2_snapshot(&hl_symbol).await?;

        let (maker_bid, maker_ask) = match maker_tob {
            Some(v) => v,
            None => {
                warn!("Maker TOB is empty for {}", maker_symbol);
                continue;
            }
        };
        let (hl_bid, hl_ask) = match hl_tob {
            Some(v) => v,
            None => {
                warn!("Hyperliquid TOB is empty for {}", hl_symbol);
                continue;
            }
        };

        recorder.add_sample(chrono::Utc::now(), maker_bid, maker_ask, hl_bid, hl_ask)?;
        samples += 1;

        let buy_edge_bps = ((hl_bid - maker_ask) / maker_ask) * 10000.0;
        let sell_edge_bps = ((maker_bid - hl_ask) / hl_ask) * 10000.0;
        info!(
            "sample={} maker({:.6}/{:.6}) hl({:.6}/{:.6}) buy_edge={:.2}bps sell_edge={:.2}bps",
            samples, maker_bid, maker_ask, hl_bid, hl_ask, buy_edge_bps, sell_edge_bps
        );
    }

    recorder.maybe_compact(true)?;
    info!(
        "Done. Captured {} sample(s). Current retained rows={}.",
        samples,
        recorder.row_count()
    );

    Ok(())
}
