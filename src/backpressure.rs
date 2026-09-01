//! Video backpressure: the M5 core (spec 2.10, 4.3.2, DR-029).
//!
//! Two independent pieces, deliberately kept separate because they have
//! different lifetimes:
//!
//! - [`BaselineTracker`]: the rolling minimum of `client_queue_delay_us`
//!   over `VIDEO_QUEUE_BASELINE_WINDOW` (default 10s). This is
//!   Channel-scoped: it persists across generation resets within a
//!   Channel and only resets on reconnection (spec 2.10) -- out of scope
//!   here since Suspended/reconnection isn't implemented yet.
//! - [`CongestionTracker`]: the Instance-scoped Streaming/Congested/Reset
//!   decision state (consecutive hard-threshold violations, hysteresis
//!   timer). A fresh Instance always starts in Streaming, so this resets
//!   on every generation reopen -- unlike the baseline.
//!
//! Both take an explicit `now_us` parameter rather than reading a clock
//! internally, so callers (including tests) control time deterministically
//! instead of needing real sleeps to exercise the 500ms hysteresis or the
//! 10s baseline window.

use std::collections::VecDeque;

/// Default threshold/window values (spec 2.10, 4.7).
pub mod defaults {
    pub const VIDEO_QUEUE_BASELINE_WINDOW_US: u64 = 10_000_000;
    pub const VIDEO_RATE_REDUCE_THRESHOLD_DELTA_US: u32 = 100_000;
    pub const MAX_VIDEO_QUEUE_DURATION_DELTA_US: u32 = 300_000;
    pub const CONGESTED_TO_STREAMING_HYSTERESIS_US: u64 = 500_000;
    pub const MAX_VIDEO_QUEUE_BYTES: u64 = 4 * 1024 * 1024;
    /// "3回連続のfeedback interval" (spec 2.10): a count of consecutive
    /// violating samples, not a wall-clock duration -- it only lines up
    /// with 300ms because feedback happens to arrive every 100ms
    /// (`TRANSPORT_FEEDBACK_INTERVAL`, spec 4.7).
    pub const HARD_THRESHOLD_CONSECUTIVE_COUNT: u32 = 3;
}

/// Rolling minimum of `client_queue_delay_us` over a trailing time window
/// (spec 2.10's `baseline`). Backed by a monotonic deque so `record` and
/// `baseline_us` are both O(1) amortized regardless of window length.
#[derive(Debug, Clone)]
pub struct BaselineTracker {
    window_us: u64,
    // Invariant: values strictly increase from front to back; the front
    // is always the minimum of everything currently in the window.
    samples: VecDeque<(u64, u32)>,
}

impl BaselineTracker {
    pub fn new(window_us: u64) -> Self {
        Self {
            window_us,
            samples: VecDeque::new(),
        }
    }

    pub fn with_default_window() -> Self {
        Self::new(defaults::VIDEO_QUEUE_BASELINE_WINDOW_US)
    }

    pub fn record(&mut self, now_us: u64, value_us: u32) {
        while let Some(&(_, back_value)) = self.samples.back() {
            if back_value >= value_us {
                self.samples.pop_back();
            } else {
                break;
            }
        }
        self.samples.push_back((now_us, value_us));
        self.evict_expired(now_us);
    }

    fn evict_expired(&mut self, now_us: u64) {
        let cutoff = now_us.saturating_sub(self.window_us);
        while let Some(&(ts, _)) = self.samples.front() {
            if ts < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// The current rolling minimum, or `None` if no samples have been
    /// recorded yet.
    pub fn baseline_us(&self) -> Option<u32> {
        self.samples.front().map(|&(_, v)| v)
    }
}

/// What the Instance's Streaming/Congested/Reset state should do in
/// response to one new sample (spec 4.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureDecision {
    /// No transition; stay in the current state.
    Continue,
    /// `Streaming -> Congested`.
    EnterCongested,
    /// `Congested -> Streaming` (hysteresis satisfied).
    ExitCongested,
    /// `Congested -> Closed(Reset)`; caller MUST reopen with
    /// `generation + 1` and a fresh self-contained IDR (spec 2.10).
    ResetStream,
}

/// Instance-scoped Streaming/Congested/Reset decision logic. Reset this
/// (via [`CongestionTracker::new`]) whenever a new Instance is created;
/// do not reset the [`BaselineTracker`] it's paired with.
#[derive(Debug, Clone)]
pub struct CongestionTracker {
    congested: bool,
    hard_violation_count: u32,
    below_soft_since_us: Option<u64>,
}

impl Default for CongestionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CongestionTracker {
    pub fn new() -> Self {
        Self {
            congested: false,
            hard_violation_count: 0,
            below_soft_since_us: None,
        }
    }

    pub fn is_congested(&self) -> bool {
        self.congested
    }

    /// Evaluates one new sample. `delta_us` is
    /// `client_queue_delay_us.saturating_sub(baseline_us)`, computed by
    /// the caller from a [`BaselineTracker`] fed the same
    /// `client_queue_delay_us` value. `app_send_queue_bytes` is the MAY
    /// supplementary signal (spec 2.10); pass 0 if unavailable, in which
    /// case only the primary (`delta_us`) signal can trigger a reset.
    pub fn evaluate(
        &mut self,
        now_us: u64,
        delta_us: u32,
        app_send_queue_bytes: u64,
    ) -> BackpressureDecision {
        use defaults::*;

        if !self.congested {
            if delta_us > VIDEO_RATE_REDUCE_THRESHOLD_DELTA_US {
                self.congested = true;
                self.hard_violation_count = 0;
                self.below_soft_since_us = None;
                return BackpressureDecision::EnterCongested;
            }
            return BackpressureDecision::Continue;
        }

        // Congested: check the hard (reset) conditions first.
        if delta_us > MAX_VIDEO_QUEUE_DURATION_DELTA_US {
            self.hard_violation_count += 1;
        } else {
            self.hard_violation_count = 0;
        }
        if self.hard_violation_count >= HARD_THRESHOLD_CONSECUTIVE_COUNT
            || app_send_queue_bytes > MAX_VIDEO_QUEUE_BYTES
        {
            // The Instance is about to be replaced; a fresh one starts a
            // fresh CongestionTracker, so no need to reset our own fields.
            return BackpressureDecision::ResetStream;
        }

        // Not resetting; check hysteresis recovery.
        if delta_us < VIDEO_RATE_REDUCE_THRESHOLD_DELTA_US {
            match self.below_soft_since_us {
                None => self.below_soft_since_us = Some(now_us),
                Some(since) => {
                    if now_us.saturating_sub(since) >= CONGESTED_TO_STREAMING_HYSTERESIS_US {
                        self.congested = false;
                        self.below_soft_since_us = None;
                        return BackpressureDecision::ExitCongested;
                    }
                }
            }
        } else {
            self.below_soft_since_us = None;
        }
        BackpressureDecision::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod baseline_tracker {
        use super::*;

        #[test]
        fn first_sample_is_the_baseline() {
            let mut b = BaselineTracker::new(10_000_000);
            b.record(0, 500);
            assert_eq!(b.baseline_us(), Some(500));
        }

        #[test]
        fn tracks_the_minimum_within_the_window() {
            let mut b = BaselineTracker::new(10_000_000);
            b.record(0, 500);
            b.record(1_000_000, 100);
            b.record(2_000_000, 300);
            assert_eq!(b.baseline_us(), Some(100));
        }

        #[test]
        fn expires_samples_outside_the_window() {
            let mut b = BaselineTracker::new(10_000_000);
            b.record(0, 50); // this sample will age out
            b.record(5_000_000, 200);
            // Now well past the first sample's 10s window.
            b.record(11_000_001, 200);
            assert_eq!(b.baseline_us(), Some(200));
        }

        #[test]
        fn new_lower_minimum_after_old_one_expires() {
            let mut b = BaselineTracker::new(10_000_000);
            b.record(0, 50);
            b.record(3_000_000, 150);
            b.record(11_000_001, 120);
            // The `50` at t=0 has aged out; among {150 (t=3s, still in
            // window), 120 (t=11.000001s)} the minimum is 120.
            assert_eq!(b.baseline_us(), Some(120));
        }

        #[test]
        fn empty_tracker_has_no_baseline() {
            let b = BaselineTracker::new(10_000_000);
            assert_eq!(b.baseline_us(), None);
        }

        #[test]
        fn stable_high_value_converges_to_itself() {
            // The exact scenario DR-029 exists for: a high-RTT link with
            // no congestion reports a constant elevated
            // client_queue_delay_us; the baseline should converge to
            // that same value (so delta ends up ~0), not stay low.
            let mut b = BaselineTracker::with_default_window();
            for i in 0..20 {
                b.record(i * 500_000, 300_000);
            }
            assert_eq!(b.baseline_us(), Some(300_000));
        }
    }

    mod congestion_tracker {
        use super::*;

        #[test]
        fn starts_not_congested() {
            assert!(!CongestionTracker::new().is_congested());
        }

        #[test]
        fn delta_over_soft_threshold_enters_congested() {
            let mut c = CongestionTracker::new();
            let decision = c.evaluate(0, 100_001, 0);
            assert_eq!(decision, BackpressureDecision::EnterCongested);
            assert!(c.is_congested());
        }

        #[test]
        fn delta_at_or_below_soft_threshold_stays_streaming() {
            let mut c = CongestionTracker::new();
            assert_eq!(c.evaluate(0, 100_000, 0), BackpressureDecision::Continue);
            assert_eq!(c.evaluate(0, 50_000, 0), BackpressureDecision::Continue);
            assert!(!c.is_congested());
        }

        #[test]
        fn recovers_after_500ms_below_soft_threshold() {
            let mut c = CongestionTracker::new();
            c.evaluate(0, 150_000, 0); // enter congested
            assert_eq!(
                c.evaluate(100_000, 50_000, 0),
                BackpressureDecision::Continue
            );
            assert_eq!(
                c.evaluate(300_000, 50_000, 0),
                BackpressureDecision::Continue
            );
            // 500ms have now elapsed since the delay first dropped below
            // threshold (t=100_000 -> t=600_000).
            assert_eq!(
                c.evaluate(600_000, 50_000, 0),
                BackpressureDecision::ExitCongested
            );
            assert!(!c.is_congested());
        }

        #[test]
        fn hysteresis_timer_resets_on_a_renewed_violation() {
            let mut c = CongestionTracker::new();
            c.evaluate(0, 150_000, 0); // enter congested
            c.evaluate(100_000, 50_000, 0); // starts recovering
            // Delay spikes back up before 500ms elapses: timer must reset.
            c.evaluate(200_000, 150_000, 0);
            // Even though 500ms have now passed since t=100_000, recovery
            // shouldn't fire because the streak was broken at t=200_000.
            assert_eq!(
                c.evaluate(650_000, 50_000, 0),
                BackpressureDecision::Continue
            );
            assert!(c.is_congested());
        }

        #[test]
        fn three_consecutive_hard_violations_trigger_reset() {
            let mut c = CongestionTracker::new();
            c.evaluate(0, 150_000, 0); // enter congested
            assert_eq!(
                c.evaluate(100_000, 300_001, 0),
                BackpressureDecision::Continue
            );
            assert_eq!(
                c.evaluate(200_000, 300_001, 0),
                BackpressureDecision::Continue
            );
            assert_eq!(
                c.evaluate(300_000, 300_001, 0),
                BackpressureDecision::ResetStream
            );
        }

        #[test]
        fn a_non_violating_sample_resets_the_hard_violation_streak() {
            let mut c = CongestionTracker::new();
            c.evaluate(0, 150_000, 0);
            c.evaluate(100_000, 300_001, 0);
            c.evaluate(200_000, 300_001, 0);
            // Streak broken here.
            c.evaluate(300_000, 150_000, 0);
            assert_eq!(
                c.evaluate(400_000, 300_001, 0),
                BackpressureDecision::Continue
            );
            assert_eq!(
                c.evaluate(500_000, 300_001, 0),
                BackpressureDecision::Continue
            );
            // Only 2 consecutive so far since the streak reset.
        }

        #[test]
        fn oversized_send_queue_triggers_reset_even_below_hard_delta_threshold() {
            let mut c = CongestionTracker::new();
            c.evaluate(0, 150_000, 0); // enter congested
            let over_limit = defaults::MAX_VIDEO_QUEUE_BYTES + 1;
            assert_eq!(
                c.evaluate(100_000, 50_000, over_limit),
                BackpressureDecision::ResetStream
            );
        }

        #[test]
        fn high_stable_delay_with_converged_baseline_never_congests() {
            // End-to-end DR-029 check at the tracker level: baseline
            // converges to a high-but-stable value, so delta stays ~0
            // and Congested is never entered, however high the absolute
            // client_queue_delay_us is.
            let mut baseline = BaselineTracker::with_default_window();
            let mut congestion = CongestionTracker::new();
            for i in 0..50u64 {
                let now_us = i * 100_000;
                let value = 300_000; // stable 300ms, e.g. a satellite link
                baseline.record(now_us, value);
                let delta = value.saturating_sub(baseline.baseline_us().unwrap());
                let decision = congestion.evaluate(now_us, delta, 0);
                assert_eq!(decision, BackpressureDecision::Continue);
            }
            assert!(!congestion.is_congested());
        }
    }
}
