use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use csv::{ReaderBuilder, WriterBuilder};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpreadSampleRow {
    pub timestamp_ms: i64,
    pub timestamp_iso: String,
    pub symbol: String,
    pub maker_exchange: String,
    pub maker_bid: f64,
    pub maker_ask: f64,
    pub maker_mid: f64,
    pub hl_bid: f64,
    pub hl_ask: f64,
    pub hl_mid: f64,
    pub mid_spread: f64,
    pub mid_spread_bps: f64,
    pub buy_edge_bps: f64,
    pub sell_edge_bps: f64,
}

impl SpreadSampleRow {
    pub fn from_prices(
        now: DateTime<Utc>,
        symbol: String,
        maker_exchange: String,
        maker_bid: f64,
        maker_ask: f64,
        hl_bid: f64,
        hl_ask: f64,
    ) -> Self {
        let maker_mid = (maker_bid + maker_ask) / 2.0;
        let hl_mid = (hl_bid + hl_ask) / 2.0;
        let mid_spread = maker_mid - hl_mid;
        let mid_spread_bps = if hl_mid > 0.0 {
            (mid_spread / hl_mid) * 10000.0
        } else {
            0.0
        };

        // BUY edge: buy maker ask, hedge sell HL bid
        let buy_edge_bps = if maker_ask > 0.0 {
            ((hl_bid - maker_ask) / maker_ask) * 10000.0
        } else {
            0.0
        };

        // SELL edge: sell maker bid, hedge buy HL ask
        let sell_edge_bps = if hl_ask > 0.0 {
            ((maker_bid - hl_ask) / hl_ask) * 10000.0
        } else {
            0.0
        };

        Self {
            timestamp_ms: now.timestamp_millis(),
            timestamp_iso: now.to_rfc3339(),
            symbol,
            maker_exchange,
            maker_bid,
            maker_ask,
            maker_mid,
            hl_bid,
            hl_ask,
            hl_mid,
            mid_spread,
            mid_spread_bps,
            buy_edge_bps,
            sell_edge_bps,
        }
    }
}

pub struct SpreadCsvRecorder {
    file_path: String,
    symbol: String,
    maker_exchange: String,
    retention: ChronoDuration,
    rows: VecDeque<SpreadSampleRow>,
    dropped_since_compaction: usize,
}

impl SpreadCsvRecorder {
    pub fn new(file_path: String, symbol: String, maker_exchange: String, retention_hours: i64) -> Self {
        Self {
            file_path,
            symbol,
            maker_exchange,
            retention: ChronoDuration::hours(retention_hours),
            rows: VecDeque::new(),
            dropped_since_compaction: 0,
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn load_existing(&mut self) -> Result<()> {
        let path = Path::new(&self.file_path);
        if !path.exists() {
            return Ok(());
        }

        let cutoff_ms = (Utc::now() - self.retention).timestamp_millis();
        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .from_path(path)
            .with_context(|| format!("Failed to open spread history file: {}", self.file_path))?;

        let mut loaded = 0usize;
        for item in reader.deserialize::<SpreadSampleRow>() {
            match item {
                Ok(row) => {
                    if row.timestamp_ms >= cutoff_ms {
                        self.rows.push_back(row);
                        loaded += 1;
                    }
                }
                Err(e) => {
                    debug!(
                        "[SPREAD_RECORDER] Skipping malformed history row in {}: {}",
                        self.file_path, e
                    );
                }
            }
        }

        // Normalize the file to only the retained window.
        self.compact_to_disk()?;
        info!(
            "[SPREAD_RECORDER] Loaded {} retained sample(s) from {}",
            loaded, self.file_path
        );
        Ok(())
    }

    pub fn add_sample(
        &mut self,
        now: DateTime<Utc>,
        maker_bid: f64,
        maker_ask: f64,
        hl_bid: f64,
        hl_ask: f64,
    ) -> Result<()> {
        let row = SpreadSampleRow::from_prices(
            now,
            self.symbol.clone(),
            self.maker_exchange.clone(),
            maker_bid,
            maker_ask,
            hl_bid,
            hl_ask,
        );

        self.rows.push_back(row.clone());
        self.prune_old(row.timestamp_ms);
        self.append_row(&row)?;
        Ok(())
    }

    pub fn maybe_compact(&mut self, force: bool) -> Result<bool> {
        if !force && self.dropped_since_compaction == 0 {
            return Ok(false);
        }
        self.compact_to_disk()?;
        self.dropped_since_compaction = 0;
        Ok(true)
    }

    fn prune_old(&mut self, now_ms: i64) {
        let cutoff_ms = now_ms - self.retention.num_milliseconds();
        while let Some(front) = self.rows.front() {
            if front.timestamp_ms >= cutoff_ms {
                break;
            }
            self.rows.pop_front();
            self.dropped_since_compaction += 1;
        }
    }

    fn append_row(&self, row: &SpreadSampleRow) -> Result<()> {
        let path = Path::new(&self.file_path);
        let has_data = path.exists()
            && fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("Failed to open spread history file for append: {}", self.file_path))?;

        let mut writer = WriterBuilder::new()
            .has_headers(!has_data)
            .from_writer(file);
        writer
            .serialize(row)
            .context("Failed to append spread history row")?;
        writer.flush().context("Failed to flush spread history writer")?;
        Ok(())
    }

    fn compact_to_disk(&self) -> Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.file_path)
            .with_context(|| format!("Failed to open spread history file for compaction: {}", self.file_path))?;

        let mut writer = WriterBuilder::new().has_headers(true).from_writer(file);
        for row in &self.rows {
            writer
                .serialize(row)
                .context("Failed to rewrite spread history row during compaction")?;
        }
        writer
            .flush()
            .context("Failed to flush spread history compaction writer")?;
        Ok(())
    }
}

pub struct SpreadRecorderService {
    pub symbol: String,
    pub maker_exchange: String,
    pub maker_prices: Arc<Mutex<(f64, f64)>>,
    pub hyperliquid_prices: Arc<Mutex<(f64, f64)>>,
    pub file_path: String,
    pub sample_interval_secs: u64,
    pub retention_hours: i64,
}

impl SpreadRecorderService {
    pub async fn run(self) {
        let sample_interval_secs = self.sample_interval_secs.max(1);
        let compact_every_ticks = (60 / sample_interval_secs).max(1);

        let mut recorder = SpreadCsvRecorder::new(
            self.file_path.clone(),
            self.symbol.clone(),
            self.maker_exchange.clone(),
            self.retention_hours,
        );
        if let Err(e) = recorder.load_existing() {
            warn!(
                "[SPREAD_RECORDER] Failed to load existing history ({}): {}",
                self.file_path, e
            );
        }

        info!(
            "[SPREAD_RECORDER] Started: symbol={}, file={}, sample={}s, retention={}h",
            self.symbol, self.file_path, sample_interval_secs, self.retention_hours
        );

        let mut ticker = interval(Duration::from_secs(sample_interval_secs));
        let mut tick_count: u64 = 0;

        loop {
            ticker.tick().await;
            tick_count += 1;

            let (maker_bid, maker_ask) = *self.maker_prices.lock();
            let (hl_bid, hl_ask) = *self.hyperliquid_prices.lock();
            if maker_bid <= 0.0 || maker_ask <= 0.0 || hl_bid <= 0.0 || hl_ask <= 0.0 {
                continue;
            }

            if let Err(e) = recorder.add_sample(Utc::now(), maker_bid, maker_ask, hl_bid, hl_ask) {
                warn!("[SPREAD_RECORDER] Failed to record sample: {}", e);
            }

            if tick_count % compact_every_ticks == 0 {
                match recorder.maybe_compact(false) {
                    Ok(true) => info!(
                        "[SPREAD_RECORDER] Compacted history to last {}h (rows={})",
                        self.retention_hours,
                        recorder.row_count()
                    ),
                    Ok(false) => {}
                    Err(e) => warn!("[SPREAD_RECORDER] Compaction failed: {}", e),
                }
            }
        }
    }
}
