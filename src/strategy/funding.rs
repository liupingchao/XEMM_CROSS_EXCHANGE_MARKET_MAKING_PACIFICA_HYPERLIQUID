use std::collections::VecDeque;
use std::time::Instant;

/// Snapshot of funding rates from both exchanges.
#[derive(Debug, Clone)]
pub struct FundingSnapshot {
    /// Binance funding rate for the current/upcoming period (per-8h, signed).
    /// Negative = shorts pay longs.
    pub maker_rate: f64,
    /// Seconds until next Binance settlement.
    pub maker_secs_to_settle: u64,
    /// Hyperliquid funding rate (per-hour, signed).
    /// Negative = shorts pay longs.
    pub hedge_rate_hourly: f64,
    /// When this snapshot was taken.
    pub captured_at: Instant,
}

/// Tracks funding carry for the position (SHORT maker + LONG hedge).
///
/// Our P&L per period:
///   Maker SHORT: +rate when rate > 0 (shorts receive), -rate when rate < 0 (shorts pay)
///   Hedge LONG:  -rate when rate > 0 (longs pay),     +rate when rate < 0 (longs receive)
///
/// For this pair (CLUSDT/xyz:CL), both rates are typically negative,
/// meaning our Maker SHORT pays ~5 bps/8h but our Hedge LONG receives ~24 bps/8h.
pub struct FundingCarryTracker {
    /// Recent net-carry observations (bps/8h equivalent).
    history: VecDeque<f64>,
    max_history: usize,
    /// Latest snapshot.
    latest: Option<FundingSnapshot>,
    /// Previous hedge hourly rate (for sudden-change detection).
    prev_hedge_rate: Option<f64>,
}

impl FundingCarryTracker {
    pub fn new(max_history: usize) -> Self {
        Self {
            history: VecDeque::with_capacity(max_history + 1),
            max_history,
            latest: None,
            prev_hedge_rate: None,
        }
    }

    /// Update with a fresh funding snapshot. Returns the net carry (bps/8h).
    pub fn update(&mut self, snapshot: FundingSnapshot) -> f64 {
        let net = self.net_carry_bps_8h(&snapshot);
        self.prev_hedge_rate = self.latest.as_ref().map(|s| s.hedge_rate_hourly);
        self.latest = Some(snapshot);

        self.history.push_back(net);
        if self.history.len() > self.max_history {
            self.history.pop_front();
        }
        net
    }

    /// Net carry in bps per 8h for our position (SHORT maker + LONG hedge).
    /// Positive = we earn money holding the position.
    fn net_carry_bps_8h(&self, snap: &FundingSnapshot) -> f64 {
        // Maker SHORT P&L = +maker_rate (positive rate benefits shorts)
        let maker_pnl = snap.maker_rate * 10000.0;
        // Hedge LONG P&L per hour = -hedge_rate (negative rate benefits longs)
        let hedge_pnl_8h = -snap.hedge_rate_hourly * 10000.0 * 8.0;
        maker_pnl + hedge_pnl_8h
    }

    /// Latest net carry (bps/8h), or 0 if no data.
    pub fn current_carry_bps(&self) -> f64 {
        self.history.back().copied().unwrap_or(0.0)
    }

    /// How many consecutive recent observations are below `threshold_bps`.
    pub fn consecutive_adverse(&self, threshold_bps: f64) -> usize {
        self.history
            .iter()
            .rev()
            .take_while(|&&c| c < threshold_bps)
            .count()
    }

    /// True if the hedge funding rate changed abruptly since last update.
    /// `threshold` is in raw rate units (e.g. 0.0005 for 5 bps).
    pub fn hedge_rate_jumped(&self, threshold: f64) -> bool {
        match (self.prev_hedge_rate, self.latest.as_ref()) {
            (Some(prev), Some(snap)) => {
                (snap.hedge_rate_hourly - prev).abs() > threshold
            }
            _ => false,
        }
    }

    /// Seconds to next Binance settlement, if known.
    pub fn maker_secs_to_settle(&self) -> Option<u64> {
        self.latest.as_ref().map(|s| {
            // Adjust for time elapsed since capture
            let elapsed = s.captured_at.elapsed().as_secs();
            s.maker_secs_to_settle.saturating_sub(elapsed)
        })
    }

    pub fn has_data(&self) -> bool {
        self.latest.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_snap(maker_rate: f64, hedge_hourly: f64) -> FundingSnapshot {
        FundingSnapshot {
            maker_rate,
            maker_secs_to_settle: 3600,
            hedge_rate_hourly: hedge_hourly,
            captured_at: Instant::now(),
        }
    }

    #[test]
    fn positive_carry() {
        let mut tracker = FundingCarryTracker::new(10);
        // Typical: maker rate -0.00053 (-5.3 bps/8h), hedge rate -0.0003 (-3.0 bps/h)
        let net = tracker.update(make_snap(-0.00053, -0.0003));
        // maker_pnl = -5.3 bps, hedge_pnl_8h = +24 bps → net = +18.7 bps
        assert!(net > 15.0, "expected positive carry, got {}", net);
    }

    #[test]
    fn adverse_detection() {
        let mut tracker = FundingCarryTracker::new(10);
        tracker.update(make_snap(0.001, 0.001)); // both positive → adverse for us
        tracker.update(make_snap(0.001, 0.001));
        tracker.update(make_snap(0.001, 0.001));
        assert_eq!(tracker.consecutive_adverse(0.0), 3);
    }
}
