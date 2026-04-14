use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared Binance API health state.
///
/// All Binance-facing components read/write this to coordinate behavior
/// during rate-limit events and IP bans.
pub struct BinanceApiHealth {
    /// Whether the Binance user stream WebSocket is alive.
    stream_alive: AtomicBool,
    /// Whether REST fill detection is operational (not rate-limited).
    rest_available: AtomicBool,
    /// Epoch millis when the current IP ban expires (0 = not banned).
    banned_until_ms: AtomicU64,
}

impl BinanceApiHealth {
    pub fn new() -> Self {
        Self {
            stream_alive: AtomicBool::new(false),
            rest_available: AtomicBool::new(true),
            banned_until_ms: AtomicU64::new(0),
        }
    }

    /// At least one fill-detection channel is working.
    pub fn is_healthy(&self) -> bool {
        !self.is_banned()
            && (self.stream_alive.load(Ordering::Acquire)
                || self.rest_available.load(Ordering::Acquire))
    }

    /// IP is currently banned by Binance.
    pub fn is_banned(&self) -> bool {
        let until = self.banned_until_ms.load(Ordering::Acquire);
        if until == 0 {
            return false;
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        now_ms < until
    }

    /// Should we skip making Binance REST API calls right now?
    pub fn should_skip_api_call(&self) -> bool {
        self.is_banned()
    }

    pub fn mark_stream_alive(&self, alive: bool) {
        self.stream_alive.store(alive, Ordering::Release);
    }

    pub fn mark_rest_available(&self, available: bool) {
        self.rest_available.store(available, Ordering::Release);
    }

    /// Record an IP ban with its expiry timestamp (epoch millis).
    pub fn mark_banned(&self, until_ms: u64) {
        self.banned_until_ms.store(until_ms, Ordering::Release);
    }
}

impl Default for BinanceApiHealth {
    fn default() -> Self {
        Self::new()
    }
}
