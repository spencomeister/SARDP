//! VideoStream Channel State Machine (spec 4.3.1), through M5:
//! `Initializing -> Live -> Recovering -> Live` (backpressure-triggered
//! generation reopen, DR-030's unbounded-retry policy). `Paused` (the
//! multi-monitor `ActiveMonitor` handoff) is out of scope: the PoC brief
//! limits this implementation to a single monitor.
//!
//! A Channel is the per-monitor concept that persists across Instance
//! reopens -- this is exactly why [`crate::backpressure::BaselineTracker`]
//! belongs with a Channel's owner, not with this SM or with the Instance
//! SM: the baseline MUST survive the `Live -> Recovering -> Live` cycle
//! (spec 2.10), even though the Instance underneath is a brand new one.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    Initializing,
    Live,
    Recovering,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelSm {
    state: ChannelState,
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
        }
    }

    pub fn state(&self) -> ChannelState {
        self.state
    }

    /// `Initializing -> Live` or `Recovering -> Live`, once an Instance
    /// reaches `Streaming` (spec 4.3.1).
    pub fn on_instance_streaming(&mut self) {
        self.state = ChannelState::Live;
    }

    /// `Live -> Recovering`, when the current Instance is abandoned via
    /// `RESET_STREAM` after a backpressure-triggered
    /// `CongestionTracker::evaluate` -> `ResetStream` decision (spec
    /// 2.10/4.3.1). Per DR-030 there is no bounded retry count or
    /// permanent give-up state: the caller keeps retrying with a new
    /// Instance (generation+1) for as long as the Connection is Active.
    pub fn on_reset(&mut self) {
        self.state = ChannelState::Recovering;
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
}
