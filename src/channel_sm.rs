//! VideoStream Channel State Machine (spec 4.3.1), scoped to M4:
//! `Initializing -> Live`. `Recovering` and `Paused` (generation-reopen
//! after backpressure, and the multi-monitor `ActiveMonitor` handoff) are
//! out of scope until M5 -- the PoC brief limits M1-M4 to a single
//! monitor with no backpressure recovery loop yet.
//!
//! A Channel is the per-monitor concept that persists across Instance
//! reopens; for M4 there is exactly one Instance per Channel, so this SM
//! only needs to observe that Instance reaching `Streaming` (spec 4.3.2).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    Initializing,
    Live,
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

    /// `Initializing -> Live`, once the first Instance reaches `Streaming`
    /// (spec 4.3.1). Idempotent: calling this again while already `Live`
    /// is a no-op (later milestones' `Recovering -> Live` on a *new*
    /// Instance's Streaming will need its own transition, not this one).
    pub fn on_instance_streaming(&mut self) {
        self.state = ChannelState::Live;
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
}
