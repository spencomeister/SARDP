//! Multi-monitor bookkeeping (Phase 2a): one [`VideoChannel`] per monitor,
//! with the client's `ActiveMonitor` (spec 2.4) driving each Channel's
//! `Live <-> Paused` transition (spec 4.3.1) as focus moves between
//! monitors. FPS/resolution reduction for `Paused` Channels is out of
//! scope here (see `channel_sm`'s module docs) -- this is purely about
//! the state transitions happening correctly.

use std::collections::HashMap;

use crate::video_channel::VideoChannel;

/// [`MonitorManager::set_active_monitor`] was asked to focus a monitor
/// that was never registered via [`MonitorManager::add_channel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownMonitor(pub u64);

/// One connection's set of per-monitor `VideoChannel`s (spec 4.3.1: a
/// Channel is a per-monitor concept), plus which one is currently focused.
#[derive(Default)]
pub struct MonitorManager {
    channels: HashMap<u64, VideoChannel>,
    active_monitor_id: Option<u64>,
}

impl MonitorManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a monitor's `Channel` (its Instance is opened elsewhere,
    /// by the caller, per `video_session`; this only tracks the resulting
    /// `VideoChannel`). The first monitor ever registered becomes the
    /// initially focused one, since a session always starts focused on
    /// *some* monitor (spec 2.4 doesn't define an "unfocused" initial
    /// state).
    pub fn add_channel(&mut self, monitor_id: u64, channel: VideoChannel) {
        if self.active_monitor_id.is_none() {
            self.active_monitor_id = Some(monitor_id);
        }
        self.channels.insert(monitor_id, channel);
    }

    pub fn channel(&self, monitor_id: u64) -> Option<&VideoChannel> {
        self.channels.get(&monitor_id)
    }

    pub fn channel_mut(&mut self, monitor_id: u64) -> Option<&mut VideoChannel> {
        self.channels.get_mut(&monitor_id)
    }

    pub fn active_monitor_id(&self) -> Option<u64> {
        self.active_monitor_id
    }

    pub fn is_registered(&self, monitor_id: u64) -> bool {
        self.channels.contains_key(&monitor_id)
    }

    /// Handles an incoming `ActiveMonitor{monitor_id}` (spec 2.4, 4.3.1):
    /// `monitor_id`'s Channel goes (back) to `Live`; every other
    /// registered Channel goes to `Paused`. Both directions are no-ops on
    /// a Channel that isn't currently `Live`/`Paused` respectively (e.g.
    /// still `Initializing` or mid-`Recovering`) -- see
    /// `ChannelSm::activate`/`deactivate`.
    pub fn set_active_monitor(&mut self, monitor_id: u64) -> Result<(), UnknownMonitor> {
        if !self.channels.contains_key(&monitor_id) {
            return Err(UnknownMonitor(monitor_id));
        }
        for (&id, channel) in self.channels.iter_mut() {
            if id == monitor_id {
                channel.activate();
            } else {
                channel.deactivate();
            }
        }
        self.active_monitor_id = Some(monitor_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_sm::ChannelState;

    fn live_channel() -> VideoChannel {
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        channel
    }

    #[test]
    fn first_registered_monitor_is_initially_active() {
        let mut manager = MonitorManager::new();
        manager.add_channel(0, live_channel());
        assert_eq!(manager.active_monitor_id(), Some(0));
    }

    #[test]
    fn switching_active_monitor_pauses_the_others_and_activates_the_target() {
        let mut manager = MonitorManager::new();
        manager.add_channel(0, live_channel());
        manager.add_channel(1, live_channel());
        manager.add_channel(2, live_channel());
        assert_eq!(manager.active_monitor_id(), Some(0));
        assert_eq!(
            manager.channel(0).unwrap().channel_state(),
            ChannelState::Live
        );

        manager
            .set_active_monitor(1)
            .expect("monitor 1 is registered");
        assert_eq!(manager.active_monitor_id(), Some(1));
        assert_eq!(
            manager.channel(0).unwrap().channel_state(),
            ChannelState::Paused
        );
        assert_eq!(
            manager.channel(1).unwrap().channel_state(),
            ChannelState::Live
        );
        assert_eq!(
            manager.channel(2).unwrap().channel_state(),
            ChannelState::Paused
        );
    }

    #[test]
    fn switching_back_reactivates_the_original_monitor() {
        let mut manager = MonitorManager::new();
        manager.add_channel(0, live_channel());
        manager.add_channel(1, live_channel());

        manager.set_active_monitor(1).unwrap();
        manager.set_active_monitor(0).unwrap();
        assert_eq!(
            manager.channel(0).unwrap().channel_state(),
            ChannelState::Live
        );
        assert_eq!(
            manager.channel(1).unwrap().channel_state(),
            ChannelState::Paused
        );
    }

    #[test]
    fn setting_an_unknown_monitor_active_is_rejected_and_changes_nothing() {
        let mut manager = MonitorManager::new();
        manager.add_channel(0, live_channel());

        assert_eq!(manager.set_active_monitor(9), Err(UnknownMonitor(9)));
        assert_eq!(manager.active_monitor_id(), Some(0));
        assert_eq!(
            manager.channel(0).unwrap().channel_state(),
            ChannelState::Live
        );
    }

    #[test]
    fn a_channel_not_yet_live_is_unaffected_by_activemonitor() {
        let mut manager = MonitorManager::new();
        manager.add_channel(0, live_channel());
        manager.add_channel(1, VideoChannel::new(0)); // still Initializing

        manager.set_active_monitor(1).unwrap();
        assert_eq!(
            manager.channel(1).unwrap().channel_state(),
            ChannelState::Initializing,
            "activate() on a not-yet-Live Channel must not fabricate a Live state"
        );
    }
}
