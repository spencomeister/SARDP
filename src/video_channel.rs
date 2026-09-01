//! Server-side orchestration for one video Channel across its Instance's
//! lifetime, tying together [`crate::backpressure`] (baseline/congestion
//! math), [`crate::channel_sm`] (Channel state), and [`crate::video_sm`]
//! (Instance state) the way a real server's per-monitor session loop
//! would (spec 2.10, 4.3.1, 4.3.2).
//!
//! The key invariant this type exists to preserve: [`BaselineTracker`]
//! persists across `prepare_reopen` calls (spec 2.10: baseline survives
//! generation resets within a Channel), while [`CongestionTracker`] and
//! [`VideoInstanceSm`] are replaced each time (a fresh Instance always
//! starts at `Streaming`, never `Congested`).

use crate::backpressure::{BackpressureDecision, BaselineTracker, CongestionTracker};
use crate::channel_sm::ChannelSm;
use crate::video_sm::{ProtocolViolation, VideoInstanceSm};

pub struct VideoChannel {
    channel_sm: ChannelSm,
    baseline: BaselineTracker,
    congestion: CongestionTracker,
    instance_sm: VideoInstanceSm,
    generation: u64,
}

impl VideoChannel {
    /// `generation` is the generation of the Instance the caller has
    /// already opened (typically 0, spec 2.10).
    pub fn new(generation: u64) -> Self {
        Self {
            channel_sm: ChannelSm::new(),
            baseline: BaselineTracker::with_default_window(),
            congestion: CongestionTracker::new(),
            instance_sm: VideoInstanceSm::new(),
            generation,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn channel_state(&self) -> crate::channel_sm::ChannelState {
        self.channel_sm.state()
    }

    pub fn instance_state(&self) -> crate::video_sm::InstanceState {
        self.instance_sm.state()
    }

    pub fn baseline_us(&self) -> Option<u32> {
        self.baseline.baseline_us()
    }

    /// `Live -> Paused` (spec 4.3.1, 2.4's `ActiveMonitor` pointing at a
    /// different monitor). A no-op outside `Live` -- see
    /// [`crate::channel_sm::ChannelSm::deactivate`].
    pub fn deactivate(&mut self) {
        self.channel_sm.deactivate();
    }

    /// `Paused -> Live` (spec 4.3.1, this monitor regaining focus). A
    /// no-op outside `Paused`.
    pub fn activate(&mut self) {
        self.channel_sm.activate();
    }

    /// Drives the current Instance's SM through `Configuring ->
    /// Streaming` and the Channel's SM to `Live`. Call once after the
    /// Instance's setup messages + first IDR have actually been sent
    /// (mirrors `video_session::open_video_instance`'s own
    /// `VideoInstanceSm` calls; this `VideoChannel` keeps a second,
    /// Channel-scoped copy of that Instance state alongside the
    /// baseline/congestion trackers it owns).
    pub fn mark_instance_streaming(&mut self) -> Result<(), ProtocolViolation> {
        self.instance_sm.on_prologue_sent()?;
        self.instance_sm.on_generation_sent()?;
        self.instance_sm.on_encoder_config_sent()?;
        self.instance_sm.on_first_idr_sent()?;
        self.channel_sm.on_instance_streaming();
        Ok(())
    }

    /// Feeds one `TransportFeedback.client_queue_delay_us` sample (spec
    /// 2.10's primary backpressure signal) into the baseline/congestion
    /// trackers, updates `VideoInstanceSm` accordingly, and returns the
    /// resulting decision.
    ///
    /// `app_send_queue_bytes` is the MAY supplementary signal; pass 0 if
    /// unavailable (this PoC does not wire a live QUIC send-buffer
    /// sensor, per the brief's guidance that the primary signal alone is
    /// sufficient -- see the module docs on `feedback_session`).
    pub fn on_feedback(
        &mut self,
        now_us: u64,
        client_queue_delay_us: u32,
        app_send_queue_bytes: u64,
    ) -> Result<BackpressureDecision, ProtocolViolation> {
        self.baseline.record(now_us, client_queue_delay_us);
        let baseline = self.baseline.baseline_us().unwrap_or(client_queue_delay_us);
        let delta = client_queue_delay_us.saturating_sub(baseline);
        let decision = self
            .congestion
            .evaluate(now_us, delta, app_send_queue_bytes);

        match decision {
            BackpressureDecision::Continue => {}
            BackpressureDecision::EnterCongested => self.instance_sm.on_congested()?,
            BackpressureDecision::ExitCongested => self.instance_sm.on_recovered()?,
            BackpressureDecision::ResetStream => {
                self.instance_sm.on_reset()?;
                self.channel_sm.on_reset();
            }
        }
        Ok(decision)
    }

    /// Call after a `ResetStream` decision, once the old stream has
    /// actually been `RESET_STREAM`ed: bumps the generation and installs
    /// a fresh `CongestionTracker`/`VideoInstanceSm` for the Instance the
    /// caller is about to open. The baseline is deliberately *not* reset
    /// (spec 2.10: it survives generation boundaries within a Channel).
    pub fn prepare_reopen(&mut self) -> u64 {
        self.generation += 1;
        self.congestion = CongestionTracker::new();
        self.instance_sm = VideoInstanceSm::new();
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backpressure::defaults;
    use crate::channel_sm::ChannelState;
    use crate::video_sm::{CloseReason, InstanceState};

    #[test]
    fn new_channel_starts_at_the_given_generation() {
        let channel = VideoChannel::new(0);
        assert_eq!(channel.generation(), 0);
        assert_eq!(channel.channel_state(), ChannelState::Initializing);
        assert_eq!(channel.instance_state(), InstanceState::Created);
    }

    #[test]
    fn mark_instance_streaming_reaches_live_and_streaming() {
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        assert_eq!(channel.channel_state(), ChannelState::Live);
        assert_eq!(channel.instance_state(), InstanceState::Streaming);
    }

    #[test]
    fn sustained_congestion_resets_and_prepare_reopen_bumps_generation() {
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();

        // Establish a low baseline first (a healthy link running for a
        // while), matching real usage: without a prior lower baseline to
        // compare against, the very first sample always defines the
        // baseline and can never itself look congested (delta=0).
        let mut now_us = 0u64;
        channel.on_feedback(now_us, 5_000, 0).unwrap();

        // Now drive delay up hard enough to enter Congested, then keep it
        // there long enough (3 consecutive violations over
        // MAX_VIDEO_QUEUE_DURATION_DELTA) to trigger a reset.
        now_us += 100_000;
        let decision = channel.on_feedback(now_us, 150_000, 0).unwrap();
        assert_eq!(decision, BackpressureDecision::EnterCongested);
        assert_eq!(channel.instance_state(), InstanceState::Congested);

        let mut last_decision = decision;
        for _ in 0..3 {
            now_us += 100_000;
            last_decision = channel.on_feedback(now_us, 400_000, 0).unwrap();
        }
        assert_eq!(last_decision, BackpressureDecision::ResetStream);
        assert_eq!(
            channel.instance_state(),
            InstanceState::Closed(CloseReason::Reset)
        );
        assert_eq!(channel.channel_state(), ChannelState::Recovering);

        let new_generation = channel.prepare_reopen();
        assert_eq!(new_generation, 1);
        assert_eq!(channel.generation(), 1);
        assert_eq!(channel.instance_state(), InstanceState::Created);

        channel.mark_instance_streaming().unwrap();
        assert_eq!(channel.channel_state(), ChannelState::Live);
        assert_eq!(channel.instance_state(), InstanceState::Streaming);
    }

    #[test]
    fn baseline_survives_reopen_but_congestion_tracker_does_not() {
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        channel.on_feedback(0, 50_000, 0).unwrap();
        let baseline_before = channel.baseline_us();
        assert_eq!(baseline_before, Some(50_000));

        // Force a reset via the byte-based fallback signal so the
        // baseline isn't disturbed by extra high-delay samples.
        channel.on_feedback(100_000, 151_000, 0).unwrap(); // enter congested (delta=101_000 > 100_000)
        channel
            .on_feedback(200_000, 50_000, defaults::MAX_VIDEO_QUEUE_BYTES + 1)
            .unwrap();
        assert_eq!(
            channel.instance_state(),
            InstanceState::Closed(CloseReason::Reset)
        );

        channel.prepare_reopen();
        // Baseline unaffected by the reopen.
        assert_eq!(channel.baseline_us(), baseline_before);

        channel.mark_instance_streaming().unwrap();
        // A fresh Instance can enter Congested again immediately; if the
        // old CongestionTracker's state had leaked through, this would
        // incorrectly still look "congested" from the start.
        assert_eq!(channel.instance_state(), InstanceState::Streaming);
    }

    #[test]
    fn high_stable_delay_never_triggers_a_reset() {
        // DR-029 at the orchestration level: a high-RTT, no-congestion
        // link never even enters Congested, so it certainly never resets.
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        for i in 0..100u64 {
            let now_us = i * 100_000;
            let decision = channel.on_feedback(now_us, 300_000, 0).unwrap();
            assert_eq!(decision, BackpressureDecision::Continue);
        }
        assert_eq!(channel.instance_state(), InstanceState::Streaming);
        assert_eq!(channel.channel_state(), ChannelState::Live);
        assert_eq!(channel.generation(), 0);
    }

    #[test]
    fn deactivate_and_activate_toggle_live_and_paused() {
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        assert_eq!(channel.channel_state(), ChannelState::Live);

        channel.deactivate();
        assert_eq!(channel.channel_state(), ChannelState::Paused);
        // The underlying Instance is untouched (spec 4.3.1: it stays
        // Streaming while Paused).
        assert_eq!(channel.instance_state(), InstanceState::Streaming);

        channel.activate();
        assert_eq!(channel.channel_state(), ChannelState::Live);
    }
}
