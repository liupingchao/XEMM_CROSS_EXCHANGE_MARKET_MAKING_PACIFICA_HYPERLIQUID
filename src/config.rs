use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StrategyMode {
    Normal,
    EventOnly,
    Dual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeTradingConfig {
    /// Target profit rate in basis points (e.g., 10.0 = 0.1%)
    pub profit_rate_bps: f64,
    /// Order notional size in USD
    pub order_notional_usd: f64,
    /// Profit cancel threshold in basis points
    pub profit_cancel_threshold_bps: f64,
    /// Order refresh interval in seconds
    pub order_refresh_interval_secs: u64,

    // --- Carry-mode fields (normal mode only, ignored in event mode) ---

    /// Z-score threshold for entry (open position when spread spikes).
    #[serde(default)]
    pub entry_z_score: f64,
    /// Rolling spread window in seconds for Z-score calculation.
    #[serde(default = "default_spread_window_secs")]
    pub spread_window_secs: u64,
    /// Max accumulated position in base units.
    #[serde(default = "default_max_position_units")]
    pub max_position_units: f64,
    /// Stop-loss: close if spread moves against entry by this many bps.
    #[serde(default = "default_max_loss_bps")]
    pub max_loss_bps: f64,
    /// Max holding time in hours before forced close.
    #[serde(default = "default_max_hold_hours")]
    pub max_hold_hours: u64,
    /// Close position when spread narrows below this (low-cost exit window).
    #[serde(default)]
    pub close_spread_bps: f64,
    /// Funding rate poll interval in seconds.
    #[serde(default = "default_funding_poll_interval_secs")]
    pub funding_poll_interval_secs: u64,
    /// Number of consecutive adverse carry observations before closing.
    #[serde(default = "default_funding_adverse_consecutive")]
    pub funding_adverse_consecutive: u64,
    /// Threshold bps below which carry is considered adverse.
    #[serde(default = "default_funding_adverse_threshold_bps")]
    pub funding_adverse_threshold_bps: f64,
    /// Timeout for maker limit close order before fallback to taker.
    #[serde(default = "default_close_limit_timeout_secs")]
    pub close_limit_timeout_secs: u64,
}

impl ModeTradingConfig {
    fn validate(&self, name: &str) -> Result<()> {
        anyhow::ensure!(
            self.profit_rate_bps > 0.0 && self.profit_rate_bps <= 500.0,
            "{}.profit_rate_bps must be in (0, 500]",
            name
        );
        anyhow::ensure!(
            self.order_notional_usd > 0.0 && self.order_notional_usd <= 1_000_000.0,
            "{}.order_notional_usd must be in (0, 1_000_000]",
            name
        );
        anyhow::ensure!(
            self.profit_cancel_threshold_bps >= 0.0 && self.profit_cancel_threshold_bps <= 500.0,
            "{}.profit_cancel_threshold_bps must be in [0, 500]",
            name
        );
        anyhow::ensure!(
            self.order_refresh_interval_secs > 0 && self.order_refresh_interval_secs <= 3600,
            "{}.order_refresh_interval_secs must be in [1, 3600]",
            name
        );
        Ok(())
    }
}

/// Application configuration loaded from config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Maker exchange backend ("pacifica" or "binance")
    #[serde(default = "default_maker_exchange")]
    pub maker_exchange: String,

    /// Trading symbol (legacy single-symbol mode, e.g., "SOL", "BTC", "ETH")
    /// Used as fallback when maker_symbol/hedge_symbol are not provided.
    pub symbol: String,

    /// Maker exchange symbol (e.g., "CLUSDT" on Binance futures)
    #[serde(default)]
    pub maker_symbol: Option<String>,

    /// Hedge exchange symbol (e.g., "xyz:CL" on Hyperliquid)
    #[serde(default)]
    pub hedge_symbol: Option<String>,

    /// Orderbook aggregation level (1, 2, 5, 10, 100, 1000)
    #[serde(default = "default_agg_level")]
    pub agg_level: u32,

    /// Maximum number of reconnection attempts
    #[serde(default = "default_reconnect_attempts")]
    pub reconnect_attempts: u32,

    /// Ping interval in seconds (keep below 60 to avoid timeout)
    #[serde(default = "default_ping_interval")]
    pub ping_interval_secs: u64,

    /// Low-latency mode: minimal logging and processing
    #[serde(default = "default_low_latency")]
    pub low_latency_mode: bool,

    /// Pacifica maker fee in basis points (e.g., 1.0 = 0.01%)
    #[serde(default = "default_pacifica_maker_fee")]
    pub pacifica_maker_fee_bps: f64,

    /// Hyperliquid taker fee in basis points (e.g., 2.5 = 0.025%)
    #[serde(default = "default_hyperliquid_taker_fee")]
    pub hyperliquid_taker_fee_bps: f64,

    /// Strategy mode:
    /// - normal: only run normal profile
    /// - event_only: only trade event profile after event trigger
    /// - dual: normal + event switching
    #[serde(default = "default_strategy_mode")]
    pub strategy_mode: StrategyMode,

    /// Target profit rate in basis points (e.g., 10.0 = 0.1%)
    /// Legacy single-profile field (used as fallback for normal_mode when missing).
    #[serde(default = "default_profit_rate")]
    pub profit_rate_bps: f64,

    /// Order notional size in USD (e.g., 20.0 = $20)
    /// Legacy single-profile field (used as fallback for normal_mode when missing).
    #[serde(default = "default_order_notional")]
    pub order_notional_usd: f64,

    /// Profit cancel threshold in basis points (cancel if profit drops by this much)
    /// Legacy single-profile field (used as fallback for normal_mode when missing).
    #[serde(default = "default_profit_cancel_threshold")]
    pub profit_cancel_threshold_bps: f64,

    /// Order refresh interval in seconds (cancel and replace if order is this old)
    /// Legacy single-profile field (used as fallback for normal_mode when missing).
    #[serde(default = "default_order_refresh_interval")]
    pub order_refresh_interval_secs: u64,

    /// Optional normal profile override. If omitted, legacy fields are used.
    #[serde(default)]
    pub normal_mode: Option<ModeTradingConfig>,

    /// Optional event profile override. If omitted, derived from legacy fields.
    #[serde(default)]
    pub event_mode: Option<ModeTradingConfig>,

    /// Event trigger threshold for |mid spread| in bps.
    #[serde(default = "default_event_trigger_mid_spread_bps")]
    pub event_trigger_mid_spread_bps: f64,

    /// Event trigger threshold for Hyperliquid bid-ask spread width in bps.
    #[serde(default = "default_event_trigger_hl_spread_bps")]
    pub event_trigger_hl_spread_bps: f64,

    /// Event trigger confirmation duration in seconds.
    #[serde(default = "default_event_trigger_confirm_secs")]
    pub event_trigger_confirm_secs: u64,

    /// Cooldown before re-arming event mode.
    #[serde(default = "default_event_rearm_cooldown_secs")]
    pub event_rearm_cooldown_secs: u64,

    /// Whether to keep running after each hedge cycle.
    #[serde(default = "default_continuous_mode")]
    pub continuous_mode: bool,

    /// Main opportunity loop interval in milliseconds.
    /// Larger values reduce API pressure and order trigger frequency.
    #[serde(default = "default_evaluation_loop_interval_ms")]
    pub evaluation_loop_interval_ms: u64,

    /// Generic cooldown after order placement failure (ms).
    #[serde(default = "default_order_failure_cooldown_ms")]
    pub order_failure_cooldown_ms: u64,

    /// Cooldown after Binance post-only reject (-5022) (ms).
    #[serde(default = "default_post_only_reject_cooldown_ms")]
    pub post_only_reject_cooldown_ms: u64,

    /// Enable event-mode exit/rebalance:
    /// 1) arm when |mid spread| >= entry threshold
    /// 2) flatten both exchanges when |mid spread| <= exit threshold for confirm duration
    #[serde(default = "default_enable_exit_rebalance")]
    pub enable_exit_rebalance: bool,

    /// Arm threshold for event-mode exit/rebalance (absolute mid spread bps).
    #[serde(default = "default_exit_rebalance_entry_spread_bps")]
    pub exit_rebalance_entry_spread_bps: f64,

    /// Exit trigger threshold for event-mode exit/rebalance (absolute mid spread bps).
    #[serde(default = "default_exit_rebalance_exit_spread_bps")]
    pub exit_rebalance_exit_spread_bps: f64,

    /// Require spread to stay under exit threshold for this many seconds before flattening.
    #[serde(default = "default_exit_rebalance_confirm_secs")]
    pub exit_rebalance_confirm_secs: u64,

    /// Cooldown between exit/rebalance attempts.
    #[serde(default = "default_exit_rebalance_cooldown_secs")]
    pub exit_rebalance_cooldown_secs: u64,

    /// Minimum absolute position size to consider non-flat when flattening.
    #[serde(default = "default_exit_rebalance_min_abs_position")]
    pub exit_rebalance_min_abs_position: f64,

    /// Hyperliquid slippage tolerance for market orders (e.g., 0.05 = 5%)
    #[serde(default = "default_hyperliquid_slippage")]
    pub hyperliquid_slippage: f64,

    /// Use Hyperliquid WebSocket for hedge market orders (default: true).
    /// When disabled, REST API is used for hedge execution.
    #[serde(default = "default_hyperliquid_use_ws_for_hedge")]
    pub hyperliquid_use_ws_for_hedge: bool,

    /// Pacifica REST API polling interval in seconds (complement to WebSocket)
    #[serde(default = "default_pacifica_rest_poll_interval")]
    pub pacifica_rest_poll_interval_secs: u64,

    /// Pacifica REST poll interval for active orders (ms).
    /// Used by REST fill detection to poll more frequently when an order is active.
    #[serde(default = "default_pacifica_active_order_rest_poll_interval")]
    pub pacifica_active_order_rest_poll_interval_ms: u64,
}

// Default values
fn default_agg_level() -> u32 {
    1
}

fn default_maker_exchange() -> String {
    "pacifica".to_string()
}

fn default_reconnect_attempts() -> u32 {
    5
}

fn default_ping_interval() -> u64 {
    15 // 15 seconds
}

fn default_low_latency() -> bool {
    false
}

fn default_pacifica_maker_fee() -> f64 {
    1.5 // 1.5 bps = 0.015%
}

fn default_hyperliquid_taker_fee() -> f64 {
    4.0 // 4 bps = 0.04%
}

fn default_strategy_mode() -> StrategyMode {
    StrategyMode::Dual
}

fn default_profit_rate() -> f64 {
    15.0 // 15 bps = 0.15%
}

fn default_order_notional() -> f64 {
    20.0 // $20 USD
}

fn default_profit_cancel_threshold() -> f64 {
    3.0 // 3 bps = 0.03%
}

fn default_order_refresh_interval() -> u64 {
    60 // 60 seconds
}

fn default_event_trigger_mid_spread_bps() -> f64 {
    100.0
}

fn default_event_trigger_hl_spread_bps() -> f64 {
    4.0
}

fn default_event_trigger_confirm_secs() -> u64 {
    2
}

fn default_event_rearm_cooldown_secs() -> u64 {
    120
}

fn default_continuous_mode() -> bool {
    true
}

fn default_evaluation_loop_interval_ms() -> u64 {
    50 // 50ms (safer than 1ms for rate limits)
}

fn default_order_failure_cooldown_ms() -> u64 {
    1500 // 1.5s
}

fn default_post_only_reject_cooldown_ms() -> u64 {
    6000 // 6s
}

fn default_enable_exit_rebalance() -> bool {
    false
}

fn default_exit_rebalance_entry_spread_bps() -> f64 {
    100.0
}

fn default_exit_rebalance_exit_spread_bps() -> f64 {
    40.0
}

fn default_exit_rebalance_confirm_secs() -> u64 {
    15
}

fn default_exit_rebalance_cooldown_secs() -> u64 {
    120
}

fn default_exit_rebalance_min_abs_position() -> f64 {
    0.01
}

fn default_hyperliquid_slippage() -> f64 {
    0.05 // 5%
}

fn default_hyperliquid_use_ws_for_hedge() -> bool {
    true
}

fn default_pacifica_rest_poll_interval() -> u64 {
    2 // 2 seconds
}

fn default_pacifica_active_order_rest_poll_interval() -> u64 {
    500 // 500 ms (safer than 100ms to avoid rate limits)
}

// --- Carry-mode defaults (for ModeTradingConfig) ---

fn default_spread_window_secs() -> u64 {
    1800 // 30 minutes
}
fn default_max_position_units() -> f64 {
    10.0
}
fn default_max_loss_bps() -> f64 {
    50.0
}
fn default_max_hold_hours() -> u64 {
    168 // 1 week
}
fn default_funding_poll_interval_secs() -> u64 {
    60
}
fn default_funding_adverse_consecutive() -> u64 {
    3
}
fn default_funding_adverse_threshold_bps() -> f64 {
    -2.0
}
fn default_close_limit_timeout_secs() -> u64 {
    30
}

impl Default for Config {
    fn default() -> Self {
        Self {
            maker_exchange: default_maker_exchange(),
            symbol: "SOL".to_string(),
            maker_symbol: None,
            hedge_symbol: None,
            agg_level: default_agg_level(),
            reconnect_attempts: default_reconnect_attempts(),
            ping_interval_secs: default_ping_interval(),
            low_latency_mode: default_low_latency(),
            pacifica_maker_fee_bps: default_pacifica_maker_fee(),
            hyperliquid_taker_fee_bps: default_hyperliquid_taker_fee(),
            strategy_mode: default_strategy_mode(),
            profit_rate_bps: default_profit_rate(),
            order_notional_usd: default_order_notional(),
            profit_cancel_threshold_bps: default_profit_cancel_threshold(),
            order_refresh_interval_secs: default_order_refresh_interval(),
            normal_mode: None,
            event_mode: None,
            event_trigger_mid_spread_bps: default_event_trigger_mid_spread_bps(),
            event_trigger_hl_spread_bps: default_event_trigger_hl_spread_bps(),
            event_trigger_confirm_secs: default_event_trigger_confirm_secs(),
            event_rearm_cooldown_secs: default_event_rearm_cooldown_secs(),
            continuous_mode: default_continuous_mode(),
            evaluation_loop_interval_ms: default_evaluation_loop_interval_ms(),
            order_failure_cooldown_ms: default_order_failure_cooldown_ms(),
            post_only_reject_cooldown_ms: default_post_only_reject_cooldown_ms(),
            enable_exit_rebalance: default_enable_exit_rebalance(),
            exit_rebalance_entry_spread_bps: default_exit_rebalance_entry_spread_bps(),
            exit_rebalance_exit_spread_bps: default_exit_rebalance_exit_spread_bps(),
            exit_rebalance_confirm_secs: default_exit_rebalance_confirm_secs(),
            exit_rebalance_cooldown_secs: default_exit_rebalance_cooldown_secs(),
            exit_rebalance_min_abs_position: default_exit_rebalance_min_abs_position(),
            hyperliquid_slippage: default_hyperliquid_slippage(),
            hyperliquid_use_ws_for_hedge: default_hyperliquid_use_ws_for_hedge(),
            pacifica_rest_poll_interval_secs: default_pacifica_rest_poll_interval(),
            pacifica_active_order_rest_poll_interval_ms: default_pacifica_active_order_rest_poll_interval(),
        }
    }
}

impl Config {
    /// Effective maker symbol (maker_symbol if set, otherwise legacy symbol).
    pub fn maker_symbol_str(&self) -> &str {
        self.maker_symbol.as_deref().unwrap_or(&self.symbol)
    }

    /// Effective hedge symbol (hedge_symbol if set, otherwise legacy symbol).
    pub fn hedge_symbol_str(&self) -> &str {
        self.hedge_symbol.as_deref().unwrap_or(&self.symbol)
    }

    pub fn normal_mode_config(&self) -> ModeTradingConfig {
        self.normal_mode.clone().unwrap_or(ModeTradingConfig {
            profit_rate_bps: self.profit_rate_bps,
            order_notional_usd: self.order_notional_usd,
            profit_cancel_threshold_bps: self.profit_cancel_threshold_bps,
            order_refresh_interval_secs: self.order_refresh_interval_secs,
        })
    }

    pub fn event_mode_config(&self) -> ModeTradingConfig {
        self.event_mode.clone().unwrap_or_else(|| {
            let profit_rate_bps = (self.profit_rate_bps + 10.0).max(self.profit_rate_bps * 1.6);
            let profit_cancel_threshold_bps =
                (self.profit_cancel_threshold_bps + 2.0).max(self.profit_cancel_threshold_bps);
            let order_refresh_interval_secs = self.order_refresh_interval_secs.max(30);
            ModeTradingConfig {
                profit_rate_bps,
                order_notional_usd: self.order_notional_usd,
                profit_cancel_threshold_bps,
                order_refresh_interval_secs,
            }
        })
    }

    /// Load configuration from a JSON file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Config = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(config)
    }

    /// Load configuration from default location (config.json)
    pub fn load_default() -> Result<Self> {
        Self::from_file("config.json")
    }

    /// Save configuration to a JSON file
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let content = serde_json::to_string_pretty(self)
            .context("Failed to serialize config")?;

        fs::write(path, content)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;

        Ok(())
    }

    /// Validate configuration values
    pub fn validate(&self) -> Result<()> {
        // Check symbol is not empty
        anyhow::ensure!(!self.symbol.is_empty(), "Symbol cannot be empty");
        anyhow::ensure!(
            !self.maker_symbol_str().trim().is_empty(),
            "maker_symbol (or symbol fallback) cannot be empty"
        );
        anyhow::ensure!(
            !self.hedge_symbol_str().trim().is_empty(),
            "hedge_symbol (or symbol fallback) cannot be empty"
        );

        // Check maker exchange value
        let maker = self.maker_exchange.to_ascii_lowercase();
        anyhow::ensure!(
            maker == "pacifica" || maker == "binance",
            "Invalid maker_exchange: {}. Must be one of: pacifica, binance",
            self.maker_exchange
        );

        // Check aggregation level is valid
        let valid_agg_levels = [1, 2, 5, 10, 100, 1000];
        anyhow::ensure!(
            valid_agg_levels.contains(&self.agg_level),
            "Invalid aggregation level: {}. Must be one of: 1, 2, 5, 10, 100, 1000",
            self.agg_level
        );

        // Check ping interval is reasonable
        anyhow::ensure!(
            self.ping_interval_secs > 0 && self.ping_interval_secs <= 30,
            "Ping interval must be between 1 and 30 seconds"
        );

        // Check reconnect attempts is reasonable
        anyhow::ensure!(
            self.reconnect_attempts > 0,
            "Reconnect attempts must be greater than 0"
        );

        // Validate financial parameters
        anyhow::ensure!(
            self.pacifica_maker_fee_bps >= 0.0,
            "pacifica_maker_fee_bps cannot be negative (got {})",
            self.pacifica_maker_fee_bps
        );
        anyhow::ensure!(
            self.hyperliquid_taker_fee_bps >= 0.0,
            "hyperliquid_taker_fee_bps cannot be negative (got {})",
            self.hyperliquid_taker_fee_bps
        );
        anyhow::ensure!(
            self.hyperliquid_slippage > 0.0 && self.hyperliquid_slippage < 0.5,
            "hyperliquid_slippage must be between 0 and 0.5 (got {})",
            self.hyperliquid_slippage
        );
        anyhow::ensure!(
            self.profit_rate_bps > 0.0 && self.profit_rate_bps <= 500.0,
            "profit_rate_bps must be in (0, 500]"
        );
        anyhow::ensure!(
            self.order_notional_usd > 0.0 && self.order_notional_usd <= 1_000_000.0,
            "order_notional_usd must be in (0, 1_000_000]"
        );
        anyhow::ensure!(
            self.profit_cancel_threshold_bps >= 0.0 && self.profit_cancel_threshold_bps <= 500.0,
            "profit_cancel_threshold_bps must be in [0, 500]"
        );
        anyhow::ensure!(
            self.order_refresh_interval_secs > 0 && self.order_refresh_interval_secs <= 3600,
            "order_refresh_interval_secs must be in [1, 3600]"
        );
        self.normal_mode_config().validate("normal_mode")?;
        self.event_mode_config().validate("event_mode")?;
        anyhow::ensure!(
            self.event_trigger_mid_spread_bps > 0.0 && self.event_trigger_mid_spread_bps <= 5000.0,
            "event_trigger_mid_spread_bps must be in (0, 5000]"
        );
        anyhow::ensure!(
            self.event_trigger_hl_spread_bps > 0.0 && self.event_trigger_hl_spread_bps <= 1000.0,
            "event_trigger_hl_spread_bps must be in (0, 1000]"
        );
        anyhow::ensure!(
            self.event_trigger_confirm_secs > 0 && self.event_trigger_confirm_secs <= 120,
            "event_trigger_confirm_secs must be in [1, 120]"
        );
        anyhow::ensure!(
            self.event_rearm_cooldown_secs <= 7200,
            "event_rearm_cooldown_secs must be <= 7200"
        );

        anyhow::ensure!(
            self.evaluation_loop_interval_ms >= 1 && self.evaluation_loop_interval_ms <= 5000,
            "evaluation_loop_interval_ms must be between 1 and 5000 ms"
        );
        anyhow::ensure!(
            self.order_failure_cooldown_ms <= 120_000,
            "order_failure_cooldown_ms must be <= 120000 ms"
        );
        anyhow::ensure!(
            self.post_only_reject_cooldown_ms <= 120_000,
            "post_only_reject_cooldown_ms must be <= 120000 ms"
        );
        anyhow::ensure!(
            self.exit_rebalance_entry_spread_bps > 0.0,
            "exit_rebalance_entry_spread_bps must be > 0"
        );
        anyhow::ensure!(
            self.exit_rebalance_exit_spread_bps > 0.0,
            "exit_rebalance_exit_spread_bps must be > 0"
        );
        anyhow::ensure!(
            self.exit_rebalance_entry_spread_bps > self.exit_rebalance_exit_spread_bps,
            "exit_rebalance_entry_spread_bps must be greater than exit_rebalance_exit_spread_bps"
        );
        anyhow::ensure!(
            self.exit_rebalance_confirm_secs <= 600,
            "exit_rebalance_confirm_secs must be <= 600"
        );
        anyhow::ensure!(
            self.exit_rebalance_cooldown_secs <= 3600,
            "exit_rebalance_cooldown_secs must be <= 3600"
        );
        anyhow::ensure!(
            self.exit_rebalance_min_abs_position >= 0.0 && self.exit_rebalance_min_abs_position <= 1000.0,
            "exit_rebalance_min_abs_position must be between 0 and 1000"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.maker_exchange, "pacifica");
        assert_eq!(config.symbol, "SOL");
        assert_eq!(config.agg_level, 1);
        assert_eq!(config.reconnect_attempts, 5);
        assert_eq!(config.ping_interval_secs, 15);
    }

    #[test]
    fn test_config_validation() {
        let mut config = Config::default();
        assert!(config.validate().is_ok());

        // Test invalid aggregation level
        config.agg_level = 3;
        assert!(config.validate().is_err());

        // Test invalid ping interval
        config.agg_level = 1;
        config.ping_interval_secs = 0;
        assert!(config.validate().is_err());

        config.ping_interval_secs = 60;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_financial_param_validation() {
        let mut config = Config::default();

        // Negative fee should fail
        config.pacifica_maker_fee_bps = -1.0;
        assert!(config.validate().is_err());
        config.pacifica_maker_fee_bps = 1.5;

        // Zero profit rate should fail
        config.profit_rate_bps = 0.0;
        assert!(config.validate().is_err());
        config.profit_rate_bps = 15.0;

        // Zero notional should fail
        config.order_notional_usd = 0.0;
        assert!(config.validate().is_err());
        config.order_notional_usd = 20.0;

        // Slippage out of range should fail
        config.hyperliquid_slippage = 0.5;
        assert!(config.validate().is_err());
        config.hyperliquid_slippage = 0.0;
        assert!(config.validate().is_err());
        config.hyperliquid_slippage = 0.05;

        // Valid config should pass
        assert!(config.validate().is_ok());
    }
}
