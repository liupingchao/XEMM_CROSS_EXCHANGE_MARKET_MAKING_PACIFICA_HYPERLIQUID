use std::collections::VecDeque;

/// Online rolling-window spread statistics for Z-score entry signals.
///
/// Maintains O(1) push/pop with incremental sum/sum_sq updates.
/// Window is time-based (configurable seconds).
pub struct SpreadStats {
    window: VecDeque<(i64, f64)>, // (timestamp_ms, mid_spread_bps)
    window_ms: i64,
    sum: f64,
    sum_sq: f64,
}

impl SpreadStats {
    pub fn new(window_secs: u64) -> Self {
        Self {
            window: VecDeque::with_capacity((window_secs as usize) + 64),
            window_ms: window_secs as i64 * 1000,
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    /// Add a new sample and expire old ones.
    pub fn push(&mut self, timestamp_ms: i64, mid_spread_bps: f64) {
        self.window.push_back((timestamp_ms, mid_spread_bps));
        self.sum += mid_spread_bps;
        self.sum_sq += mid_spread_bps * mid_spread_bps;

        let cutoff = timestamp_ms - self.window_ms;
        while let Some(&(ts, val)) = self.window.front() {
            if ts >= cutoff {
                break;
            }
            self.window.pop_front();
            self.sum -= val;
            self.sum_sq -= val * val;
        }
    }

    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// At least `min_samples` in the window.
    pub fn is_ready(&self, min_samples: usize) -> bool {
        self.window.len() >= min_samples
    }

    pub fn mean(&self) -> f64 {
        let n = self.window.len();
        if n == 0 {
            return 0.0;
        }
        self.sum / n as f64
    }

    pub fn std(&self) -> f64 {
        let n = self.window.len();
        if n < 2 {
            return 0.0;
        }
        let mean = self.sum / n as f64;
        let var = (self.sum_sq / n as f64) - mean * mean;
        var.max(0.0).sqrt()
    }

    /// Z-score of `current` relative to the rolling window.
    /// Returns 0.0 if not enough data or std is near zero.
    pub fn z_score(&self, current: f64) -> f64 {
        let s = self.std();
        if s < 0.01 || self.window.len() < 60 {
            return 0.0;
        }
        (current - self.mean()) / s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_stats() {
        let mut ss = SpreadStats::new(10); // 10 second window
        // Push 5 samples at 1-second intervals
        for i in 0..5 {
            ss.push(i * 1000, 20.0 + i as f64);
        }
        assert_eq!(ss.len(), 5);
        // mean of [20, 21, 22, 23, 24] = 22.0
        assert!((ss.mean() - 22.0).abs() < 0.001);
        assert!(ss.std() > 0.0);
    }

    #[test]
    fn expiry() {
        let mut ss = SpreadStats::new(5); // 5 second window
        ss.push(0, 10.0);
        ss.push(1000, 20.0);
        ss.push(6000, 30.0); // pushes out the first sample
        assert_eq!(ss.len(), 2); // only 1000 and 6000 remain
    }

    #[test]
    fn z_score_positive_spike() {
        let mut ss = SpreadStats::new(1800);
        // Fill with 100 samples of ~20 bps
        for i in 0..100 {
            ss.push(i * 1000, 20.0);
        }
        // A spike to 28 should have positive Z-score
        let z = ss.z_score(28.0);
        // std is ~0 since all same value, z_score returns 0 when std < 0.01
        // Add some noise
        let mut ss2 = SpreadStats::new(1800);
        for i in 0..200 {
            ss2.push(i * 1000, 20.0 + (i % 3) as f64 - 1.0); // 19, 20, 21 cycle
        }
        let z2 = ss2.z_score(28.0);
        assert!(z2 > 2.0, "z_score should be > 2 for large spike, got {}", z2);
    }
}
