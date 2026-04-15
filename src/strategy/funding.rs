use std::collections::VecDeque;
use std::time::Instant;

/// Direction of a carry position across two exchanges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarryDirection {
    /// Maker is expensive: SHORT maker + LONG hedge.
    ShortMakerLongHedge,
    /// Hedge is expensive: LONG maker + SHORT hedge.
    LongMakerShortHedge,
}

/// Current regime assessment for carry trading.
#[derive(Debug, Clone)]
pub struct CarryRegime {
    /// Optimal direction given current spread sign.
    pub direction: CarryDirection,
    /// Net carry for this direction (bps/8h). Positive = profitable to hold.
    pub net_carry_bps: f64,
    /// Absolute mid spread (bps).
    pub spread_bps: f64,
    /// Carry trend: positive = carry improving, negative = deteriorating.
    pub funding_trend: f64,
    /// True when net_carry > 0 and spread is meaningful.
    pub is_favorable: bool,
}

/// Snapshot of funding rates from both exchanges.
#[derive(Debug, Clone)]
pub struct FundingSnapshot {
    /// Maker (Binance) funding rate (per-8h, signed).
    /// Positive = shorts receive; negative = shorts pay.
    pub maker_rate: f64,
    /// Seconds until next Binance settlement.
    pub maker_secs_to_settle: u64,
    /// Hedge (Hyperliquid) funding rate (per-hour, signed).
    /// Positive = shorts receive; negative = shorts pay.
    pub hedge_rate_hourly: f64,
    /// When this snapshot was taken.
    pub captured_at: Instant,
}

/// Direction-agnostic funding carry tracker.
///
/// Computes net carry for the **optimal** direction (short the expensive side)
/// rather than assuming a fixed position.
pub struct FundingCarryTracker {
    /// Recent net-carry observations (bps/8h, for the evaluated direction).
    history: VecDeque<f64>,
    max_history: usize,
    /// Latest snapshot.
    latest: Option<FundingSnapshot>,
    /// Previous hedge hourly rate (for sudden-change detection).
    prev_hedge_rate: Option<f64>,
    /// Direction of the last evaluation (tracks regime changes).
    last_direction: Option<CarryDirection>,
}

impl FundingCarryTracker {
    pub fn new(max_history: usize) -> Self {
        Self {
            history: VecDeque::with_capacity(max_history + 1),
            max_history,
            latest: None,
            prev_hedge_rate: None,
            last_direction: None,
        }
    }

    /// Update with a fresh funding snapshot. Returns the net carry (bps/8h)
    /// for the SHORT-maker + LONG-hedge direction (legacy, for backwards compat).
    pub fn update(&mut self, snapshot: FundingSnapshot) -> f64 {
        let net = Self::carry_for_direction(
            CarryDirection::ShortMakerLongHedge,
            snapshot.maker_rate,
            snapshot.hedge_rate_hourly,
        );
        self.prev_hedge_rate = self.latest.as_ref().map(|s| s.hedge_rate_hourly);
        self.latest = Some(snapshot);

        self.history.push_back(net);
        if self.history.len() > self.max_history {
            self.history.pop_front();
        }
        net
    }

    /// Evaluate the current carry regime given live spread and rates.
    ///
    /// Determines optimal direction, carry magnitude, and trend.
    pub fn evaluate_regime(
        &self,
        mid_spread_bps: f64,
        min_entry_spread_bps: f64,
        min_carry_bps: f64,
    ) -> CarryRegime {
        let (maker_rate, hedge_rate) = self
            .latest
            .as_ref()
            .map(|s| (s.maker_rate, s.hedge_rate_hourly))
            .unwrap_or((0.0, 0.0));

        // Direction: short the expensive side
        let direction = if mid_spread_bps >= 0.0 {
            CarryDirection::ShortMakerLongHedge
        } else {
            CarryDirection::LongMakerShortHedge
        };

        let net_carry_bps = Self::carry_for_direction(direction, maker_rate, hedge_rate);
        let spread_bps = mid_spread_bps.abs();
        let funding_trend = self.funding_trend(6);
        let is_favorable =
            net_carry_bps >= min_carry_bps && spread_bps >= min_entry_spread_bps;

        CarryRegime {
            direction,
            net_carry_bps,
            spread_bps,
            funding_trend,
            is_favorable,
        }
    }

    /// Net carry (bps/8h) for a given direction.
    ///
    /// Funding convention: positive rate → shorts receive, longs pay.
    fn carry_for_direction(
        direction: CarryDirection,
        maker_rate: f64,
        hedge_rate_hourly: f64,
    ) -> f64 {
        match direction {
            CarryDirection::ShortMakerLongHedge => {
                // We are SHORT maker → PnL = +maker_rate
                // We are LONG hedge  → PnL = -hedge_rate (longs pay positive rate)
                let maker_pnl = maker_rate * 10000.0;
                let hedge_pnl = -hedge_rate_hourly * 10000.0 * 8.0;
                maker_pnl + hedge_pnl
            }
            CarryDirection::LongMakerShortHedge => {
                // We are LONG maker  → PnL = -maker_rate
                // We are SHORT hedge → PnL = +hedge_rate
                let maker_pnl = -maker_rate * 10000.0;
                let hedge_pnl = hedge_rate_hourly * 10000.0 * 8.0;
                maker_pnl + hedge_pnl
            }
        }
    }

    /// Carry trend: average of recent N - average of previous N.
    /// Positive = carry is improving; negative = deteriorating.
    pub fn funding_trend(&self, half_window: usize) -> f64 {
        let n = self.history.len();
        if n < half_window * 2 {
            return 0.0;
        }
        let recent: f64 = self.history.iter().rev().take(half_window).sum::<f64>()
            / half_window as f64;
        let previous: f64 = self
            .history
            .iter()
            .rev()
            .skip(half_window)
            .take(half_window)
            .sum::<f64>()
            / half_window as f64;
        recent - previous
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
            let elapsed = s.captured_at.elapsed().as_secs();
            s.maker_secs_to_settle.saturating_sub(elapsed)
        })
    }

    pub fn has_data(&self) -> bool {
        self.latest.is_some()
    }

    pub fn last_direction(&self) -> Option<CarryDirection> {
        self.last_direction
    }

    /// Record the direction we actually positioned in.
    pub fn set_positioned_direction(&mut self, dir: CarryDirection) {
        self.last_direction = Some(dir);
    }

    pub fn clear_direction(&mut self) {
        self.last_direction = None;
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
    fn positive_carry_short_maker() {
        let mut tracker = FundingCarryTracker::new(10);
        // maker rate -0.00053 (-5.3 bps/8h), hedge rate -0.0003 (-3.0 bps/h)
        let net = tracker.update(make_snap(-0.00053, -0.0003));
        // SHORT maker: pays 5.3 bps. LONG hedge: receives 3*8=24 bps. net = +18.7
        assert!(net > 15.0, "expected positive carry, got {}", net);
    }

    #[test]
    fn direction_selection() {
        let mut tracker = FundingCarryTracker::new(10);
        tracker.update(make_snap(-0.00053, -0.0003));

        // BN expensive (spread > 0): should be ShortMakerLongHedge
        let r = tracker.evaluate_regime(20.0, 8.0, 0.0);
        assert_eq!(r.direction, CarryDirection::ShortMakerLongHedge);
        assert!(r.is_favorable);

        // HL expensive (spread < 0): should be LongMakerShortHedge
        let r2 = tracker.evaluate_regime(-20.0, 8.0, 0.0);
        assert_eq!(r2.direction, CarryDirection::LongMakerShortHedge);
    }

    #[test]
    fn funding_trend_detection() {
        let mut tracker = FundingCarryTracker::new(20);
        // 6 observations at +15, then 6 at +5 → trend = 5-15 = -10 (deteriorating)
        for _ in 0..6 {
            tracker.update(make_snap(-0.0015, -0.0003)); // high carry
        }
        for _ in 0..6 {
            tracker.update(make_snap(-0.0005, -0.00010)); // low carry
        }
        let trend = tracker.funding_trend(6);
        assert!(trend < -5.0, "expected negative trend, got {}", trend);
    }

    #[test]
    fn adverse_detection() {
        let mut tracker = FundingCarryTracker::new(10);
        tracker.update(make_snap(0.001, 0.001));
        tracker.update(make_snap(0.001, 0.001));
        tracker.update(make_snap(0.001, 0.001));
        assert_eq!(tracker.consecutive_adverse(0.0), 3);
    }
}
