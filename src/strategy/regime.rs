use std::time::{Duration, Instant};

use crate::config::StrategyMode;
use crate::strategy::OrderSide;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryProfile {
    Normal,
    Event,
}

#[derive(Debug, Clone, Copy)]
pub struct RegimeDecision {
    pub profile: EntryProfile,
    pub direction_filter: Option<OrderSide>,
    pub suppress_entries: bool,
    pub event_active: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RegimeSettings {
    pub strategy_mode: StrategyMode,
    pub event_trigger_mid_spread_bps: f64,
    pub event_trigger_hl_spread_bps: f64,
    pub event_trigger_confirm_secs: u64,
}

#[derive(Debug, Clone)]
pub struct RegimeController {
    settings: RegimeSettings,
    event_active: bool,
    event_candidate_since: Option<Instant>,
    next_event_allowed_at: Instant,
}

impl RegimeController {
    pub fn new(settings: RegimeSettings) -> Self {
        Self {
            settings,
            event_active: false,
            event_candidate_since: None,
            next_event_allowed_at: Instant::now(),
        }
    }

    pub fn event_active(&self) -> bool {
        self.event_active
    }

    pub fn clear_event_with_cooldown(&mut self, cooldown_secs: u64) {
        self.event_active = false;
        self.event_candidate_since = None;
        self.next_event_allowed_at = Instant::now() + Duration::from_secs(cooldown_secs.max(1));
    }

    pub fn update(
        &mut self,
        now: Instant,
        mid_spread_bps: f64,
        hl_spread_bps: f64,
    ) -> RegimeDecision {
        if matches!(self.settings.strategy_mode, StrategyMode::Normal) {
            self.event_active = false;
            self.event_candidate_since = None;
            return RegimeDecision {
                profile: EntryProfile::Normal,
                direction_filter: None,
                suppress_entries: false,
                event_active: false,
            };
        }

        let abs_mid_spread_bps = mid_spread_bps.abs();
        let trigger_ready = now >= self.next_event_allowed_at
            && abs_mid_spread_bps >= self.settings.event_trigger_mid_spread_bps
            && hl_spread_bps >= self.settings.event_trigger_hl_spread_bps;

        if !self.event_active && trigger_ready {
            if self.event_candidate_since.is_none() {
                self.event_candidate_since = Some(now);
            }

            if let Some(since) = self.event_candidate_since {
                if since.elapsed() >= Duration::from_secs(self.settings.event_trigger_confirm_secs.max(1)) {
                    self.event_active = true;
                    self.event_candidate_since = None;
                }
            }
        } else if !trigger_ready {
            self.event_candidate_since = None;
        }

        let direction_filter = if self.event_active {
            if mid_spread_bps >= 0.0 {
                Some(OrderSide::Sell)
            } else {
                Some(OrderSide::Buy)
            }
        } else {
            None
        };

        match self.settings.strategy_mode {
            StrategyMode::EventOnly => RegimeDecision {
                profile: EntryProfile::Event,
                direction_filter,
                suppress_entries: !self.event_active,
                event_active: self.event_active,
            },
            StrategyMode::Dual => RegimeDecision {
                profile: if self.event_active {
                    EntryProfile::Event
                } else {
                    EntryProfile::Normal
                },
                direction_filter,
                suppress_entries: false,
                event_active: self.event_active,
            },
            StrategyMode::Normal => unreachable!("handled above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_mode_never_enters_event() {
        let settings = RegimeSettings {
            strategy_mode: StrategyMode::Normal,
            event_trigger_mid_spread_bps: 50.0,
            event_trigger_hl_spread_bps: 4.0,
            event_trigger_confirm_secs: 1,
        };
        let mut controller = RegimeController::new(settings);
        let d = controller.update(Instant::now(), 120.0, 10.0);
        assert_eq!(d.profile, EntryProfile::Normal);
        assert!(!d.event_active);
    }

    #[test]
    fn dual_mode_enters_event_after_confirm() {
        let settings = RegimeSettings {
            strategy_mode: StrategyMode::Dual,
            event_trigger_mid_spread_bps: 50.0,
            event_trigger_hl_spread_bps: 4.0,
            event_trigger_confirm_secs: 1,
        };
        let mut controller = RegimeController::new(settings);
        let now = Instant::now();
        let d1 = controller.update(now, 120.0, 10.0);
        assert_eq!(d1.profile, EntryProfile::Normal);
        std::thread::sleep(Duration::from_millis(1100));
        let d2 = controller.update(Instant::now(), 120.0, 10.0);
        assert_eq!(d2.profile, EntryProfile::Event);
        assert!(d2.event_active);
        assert_eq!(d2.direction_filter, Some(OrderSide::Sell));
    }
}
