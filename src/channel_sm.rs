//! VideoStream Channel State Machine (spec 4.3.1):
//! `Initializing -> Live -> Recovering -> Live` (backpressure-triggered
//! generation reopen, DR-030's unbounded-retry policy), plus `Live <->
//! Paused` for the multi-monitor `ActiveMonitor` handoff (Phase 2a; see
//! [`crate::monitor_manager`] for the per-connection multi-Channel
//! bookkeeping this drives). Per spec 4.3.1: "Paused状態では、下位の
//! Instanceは Streaming のまま維持し、エンコーダが送出レート・解像度のみを
//! 下げる(generationは変えない)" -- this PoC keeps that FPS/resolution
//! reduction out of scope (only the state transition itself matters here);
//! `Recovering`, unlike `Paused`, always bumps the generation.
//!
//! A Channel is the per-monitor concept that persists across Instance
//! reopens -- this is exactly why [`crate::backpressure::BaselineTracker`]
//! belongs with a Channel's owner, not with this SM or with the Instance
//! SM: the baseline MUST survive the `Live -> Recovering -> Live` cycle
//! (spec 2.10), even though the Instance underneath is a brand new one.
//!
//! DR-030's exponential backoff (1s, 2s, 4s, 8s, 16s, 30s, then 30s
//! plateau) governs *retries within one Recovering episode*: per spec
//! 4.3.1, the transition into `Recovering` already includes opening the
//! first replacement Instance immediately ("新Instance生成" is part of
//! `Live -> Recovering` itself) -- there is no backoff before that first
//! attempt. The backoff only applies if *that* attempt also fails to
//! reach `Streaming` within `VIDEO_RECOVERY_TIMEOUT` (5s), gating when
//! the *next* attempt after that may start. Like `backpressure.rs`, time
//! is an explicit `now_us` parameter rather than a clock read internally,
//! so the wait can be tested deterministically instead of with real
//! sleeps.

/// DR-030's backoff sequence and plateau (spec 4.3.1).
pub mod defaults {
    use std::time::Duration;

    pub const RECOVERY_BACKOFF_SEQUENCE_US: [u64; 6] = [
        1_000_000, 2_000_000, 4_000_000, 8_000_000, 16_000_000, 30_000_000,
    ];
    pub const RECOVERY_BACKOFF_PLATEAU_US: u64 = 30_000_000;
    /// Time allowed for a Recovering episode's retry Instance to reach
    /// `Streaming` before `on_retry_timeout` applies the next backoff step
    /// (spec 4.3.1/4.7).
    pub const VIDEO_RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    Initializing,
    Live,
    Recovering,
    /// Not the focused monitor (spec 2.4/4.3.1: `ActiveMonitor` points
    /// elsewhere). The underlying Instance stays `Streaming`.
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelSm {
    state: ChannelState,
    /// Count of consecutive retry attempts that have failed to reach
    /// `Streaming` within the current Recovering episode. Indexes into
    /// `RECOVERY_BACKOFF_SEQUENCE_US`; 0 means no failed retry yet within
    /// this episode (the first retry, opened as part of entering
    /// `Recovering`, hasn't been judged yet).
    retry_attempt: u32,
    /// `None`: a retry may start immediately. `Some(t)`: a retry may only
    /// start at or after `t` (DR-030 backoff in effect).
    next_retry_allowed_at_us: Option<u64>,
}

impl Default for ChannelSm {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelSm {
    pub fn new() -> Self {
        Self {
            state: ChannelState::Initializing,
            retry_attempt: 0,
            next_retry_allowed_at_us: None,
        }
    }

    pub fn state(&self) -> ChannelState {
        self.state
    }

    /// `Initializing -> Live` or `Recovering -> Live`, once an Instance
    /// reaches `Streaming` (spec 4.3.1). A successful recovery clears the
    /// backoff: the *next* time this Channel needs to recover from
    /// scratch is an unrelated episode that starts back at the 1s step.
    pub fn on_instance_streaming(&mut self) {
        self.state = ChannelState::Live;
        self.retry_attempt = 0;
        self.next_retry_allowed_at_us = None;
    }

    /// `Live -> Recovering`, when the current Instance is abandoned via
    /// `RESET_STREAM` after a backpressure-triggered
    /// `CongestionTracker::evaluate` -> `ResetStream` decision (spec
    /// 2.10/4.3.1). Per DR-030 there is no bounded retry count or
    /// permanent give-up state: the caller keeps retrying with a new
    /// Instance (generation+1) for as long as the Connection is Active.
    ///
    /// The first retry Instance is opened immediately (spec 4.3.1: it's
    /// part of this very transition), so `may_retry` returns `true` right
    /// after this call; backoff only applies from `on_retry_timeout`
    /// onward.
    pub fn on_reset(&mut self) {
        self.state = ChannelState::Recovering;
    }

    /// Spec 4.3.1: `VIDEO_RECOVERY_TIMEOUT`(5秒)超過 -- the most recent
    /// retry Instance failed to reach `Streaming` in time. Stays in
    /// `Recovering` and schedules the next allowed retry per DR-030's
    /// backoff sequence, advancing one step further into it.
    pub fn on_retry_timeout(&mut self, now_us: u64) {
        let delay_us = defaults::RECOVERY_BACKOFF_SEQUENCE_US
            .get(self.retry_attempt as usize)
            .copied()
            .unwrap_or(defaults::RECOVERY_BACKOFF_PLATEAU_US);
        self.next_retry_allowed_at_us = Some(now_us + delay_us);
        self.retry_attempt += 1;
    }

    /// Whether a new retry Instance may be opened at `now_us`. Always
    /// `true` before any `on_retry_timeout` call in the current episode
    /// (the first retry is unconditional); after that, gated by the
    /// DR-030 backoff `on_retry_timeout` scheduled.
    pub fn may_retry(&self, now_us: u64) -> bool {
        match self.next_retry_allowed_at_us {
            None => true,
            Some(t) => now_us >= t,
        }
    }

    /// Number of consecutive failed retries in the current Recovering
    /// episode (0 if none have failed yet, or after a successful
    /// recovery reset it).
    pub fn retry_attempt(&self) -> u32 {
        self.retry_attempt
    }

    /// The wall-clock time (if any) before which `may_retry` returns
    /// `false`.
    pub fn next_retry_allowed_at_us(&self) -> Option<u64> {
        self.next_retry_allowed_at_us
    }

    /// `Live -> Paused`, when this Channel's monitor loses focus (spec
    /// 4.3.1, driven by an incoming `ActiveMonitor` naming a different
    /// monitor). A no-op outside `Live`: losing focus while
    /// `Initializing`/`Recovering` doesn't cancel whatever's already
    /// happening, it only matters once/if the Channel would otherwise be
    /// `Live` while unfocused.
    pub fn deactivate(&mut self) {
        if self.state == ChannelState::Live {
            self.state = ChannelState::Paused;
        }
    }

    /// `Paused -> Live`, when this Channel's monitor regains focus. A
    /// no-op outside `Paused`.
    pub fn activate(&mut self) {
        if self.state == ChannelState::Paused {
            self.state = ChannelState::Live;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_initializing() {
        assert_eq!(ChannelSm::new().state(), ChannelState::Initializing);
    }

    #[test]
    fn instance_streaming_transitions_to_live() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        assert_eq!(sm.state(), ChannelState::Live);
    }

    #[test]
    fn reset_transitions_live_to_recovering() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.on_reset();
        assert_eq!(sm.state(), ChannelState::Recovering);
    }

    #[test]
    fn new_instance_streaming_recovers_from_recovering_to_live() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.on_reset();
        sm.on_instance_streaming();
        assert_eq!(sm.state(), ChannelState::Live);
    }

    #[test]
    fn first_retry_after_reset_is_immediate() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.on_reset();
        // No on_retry_timeout call yet: unconditional first retry.
        assert!(sm.may_retry(0));
        assert_eq!(sm.retry_attempt(), 0);
    }

    #[test]
    fn retry_timeout_blocks_until_the_backoff_elapses() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.on_reset();
        sm.on_retry_timeout(1_000); // first backoff step: 1s
        assert!(!sm.may_retry(1_000 + 999_999));
        assert!(sm.may_retry(1_000 + 1_000_000));
    }

    #[test]
    fn backoff_sequence_matches_dr_030() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.on_reset();

        let expected_delays_us: [u64; 8] = [
            1_000_000, 2_000_000, 4_000_000, 8_000_000, 16_000_000, 30_000_000, 30_000_000,
            30_000_000, // plateau: repeats indefinitely past the 6th
        ];
        let mut now_us = 0u64;
        for expected_delay in expected_delays_us {
            sm.on_retry_timeout(now_us);
            assert_eq!(sm.next_retry_allowed_at_us(), Some(now_us + expected_delay));
            now_us += expected_delay;
        }
    }

    #[test]
    fn retry_attempt_counter_advances_with_each_timeout() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.on_reset();
        assert_eq!(sm.retry_attempt(), 0);
        sm.on_retry_timeout(0);
        assert_eq!(sm.retry_attempt(), 1);
        sm.on_retry_timeout(1_000_000);
        assert_eq!(sm.retry_attempt(), 2);
    }

    #[test]
    fn successful_recovery_resets_the_backoff_for_the_next_episode() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.on_reset();
        sm.on_retry_timeout(0); // now deep into a 1s backoff
        assert_eq!(sm.retry_attempt(), 1);

        sm.on_instance_streaming(); // recovery succeeds
        assert_eq!(sm.retry_attempt(), 0);
        assert_eq!(sm.next_retry_allowed_at_us(), None);

        // A brand new episode starts fresh, not mid-backoff from before.
        sm.on_reset();
        assert!(sm.may_retry(0));
    }

    #[test]
    fn deactivate_transitions_live_to_paused() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.deactivate();
        assert_eq!(sm.state(), ChannelState::Paused);
    }

    #[test]
    fn activate_transitions_paused_to_live() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.deactivate();
        sm.activate();
        assert_eq!(sm.state(), ChannelState::Live);
    }

    #[test]
    fn deactivate_outside_live_is_a_no_op() {
        let mut sm = ChannelSm::new();
        assert_eq!(sm.state(), ChannelState::Initializing);
        sm.deactivate();
        assert_eq!(sm.state(), ChannelState::Initializing);

        sm.on_instance_streaming();
        sm.on_reset();
        assert_eq!(sm.state(), ChannelState::Recovering);
        sm.deactivate();
        assert_eq!(
            sm.state(),
            ChannelState::Recovering,
            "losing focus mid-recovery must not cancel the recovery"
        );
    }

    #[test]
    fn activate_outside_paused_is_a_no_op() {
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        assert_eq!(sm.state(), ChannelState::Live);
        sm.activate(); // already Live
        assert_eq!(sm.state(), ChannelState::Live);
    }

    #[test]
    fn reset_while_paused_overrides_pause_and_enters_recovering() {
        // A Paused Channel's Instance can still be reset by backpressure
        // (spec 4.3.1 doesn't exempt unfocused monitors from congestion
        // handling); on_reset() is unconditional, matching the spec's
        // "任意の状態" framing for the transitions it does specify -- but
        // Paused itself isn't one of the states on_reset lists a source
        // for, so this documents the actual (Paused-clobbering) behavior.
        let mut sm = ChannelSm::new();
        sm.on_instance_streaming();
        sm.deactivate();
        assert_eq!(sm.state(), ChannelState::Paused);
        sm.on_reset();
        assert_eq!(sm.state(), ChannelState::Recovering);
    }
}
