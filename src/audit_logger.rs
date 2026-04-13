use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use csv::WriterBuilder;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::path::Path;

use crate::strategy::OrderSide;

#[derive(Debug, Serialize)]
pub struct OrderRecord {
    pub timestamp: String,
    pub symbol: String,
    pub maker_exchange: String,
    pub side: String,
    pub size: f64,
    pub price: f64,
    pub notional_usd: f64,
    pub order_id: Option<String>,
    pub client_order_id: Option<String>,
    pub source: String,
}

impl OrderRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        timestamp: DateTime<Utc>,
        symbol: String,
        maker_exchange: String,
        side: OrderSide,
        size: f64,
        price: f64,
        order_id: Option<String>,
        client_order_id: Option<String>,
        source: String,
    ) -> Self {
        Self {
            timestamp: timestamp.to_rfc3339(),
            symbol,
            maker_exchange,
            side: side.as_str().to_uppercase(),
            size,
            price,
            notional_usd: size * price,
            order_id,
            client_order_id,
            source,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FillRecord {
    pub timestamp: String,
    pub symbol: String,
    pub maker_exchange: String,
    pub side: String,
    pub fill_size: f64,
    pub fill_price: f64,
    pub fill_notional_usd: f64,
    pub is_full_fill: bool,
    pub client_order_id: Option<String>,
    pub exchange_order_id: Option<String>,
    pub source: String,
}

impl FillRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        timestamp: DateTime<Utc>,
        symbol: String,
        maker_exchange: String,
        side: OrderSide,
        fill_size: f64,
        fill_price: f64,
        is_full_fill: bool,
        client_order_id: Option<String>,
        exchange_order_id: Option<String>,
        source: String,
    ) -> Self {
        Self {
            timestamp: timestamp.to_rfc3339(),
            symbol,
            maker_exchange,
            side: side.as_str().to_uppercase(),
            fill_size,
            fill_price,
            fill_notional_usd: fill_size * fill_price,
            is_full_fill,
            client_order_id,
            exchange_order_id,
            source,
        }
    }
}

pub fn order_file_for_symbol(symbol: &str) -> String {
    format!("{}_orders.csv", sanitize_symbol(symbol))
}

pub fn fill_file_for_symbol(symbol: &str) -> String {
    format!("{}_fills.csv", sanitize_symbol(symbol))
}

pub fn log_order(file_path: &str, record: &OrderRecord) -> Result<()> {
    append_record(file_path, record).with_context(|| format!("Failed to log order to {}", file_path))
}

pub fn log_fill(file_path: &str, record: &FillRecord) -> Result<()> {
    append_record(file_path, record).with_context(|| format!("Failed to log fill to {}", file_path))
}

fn append_record<T: Serialize>(file_path: &str, record: &T) -> Result<()> {
    let path = Path::new(file_path);
    let has_data = path.exists() && fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open audit CSV file: {}", file_path))?;

    let mut writer = WriterBuilder::new()
        .has_headers(!has_data)
        .from_writer(file);
    writer
        .serialize(record)
        .context("Failed to append audit CSV row")?;
    writer.flush().context("Failed to flush audit CSV writer")?;
    Ok(())
}

fn sanitize_symbol(symbol: &str) -> String {
    symbol
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
