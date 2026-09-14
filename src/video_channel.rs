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
use crate::messages::KeyframeRequest;
use crate::video_sm::{ProtocolViolation, VideoInstanceSm};

pub mod defaults {
    /// Minimum spacing between two reopens the server performs in
    /// response to client `KeyframeRequest`s (spec 2.10 / 4.5, a MAY):
    /// 1 second, the same value as the client queue circuit breaker's
    /// trip threshold ([`crate::queue_circuit_breaker::defaults::TRIP_THRESHOLD_US`]).
    ///
    /// Why that value: after a reopen the client discards the old
    /// generation's backlog, so its queue age restarts from 0 and a
    /// well-behaved breaker cannot legitimately trip again until the
    /// *new* generation has itself been queued for more than the trip
    /// threshold. Any request arriving sooner is a duplicate, a request
    /// that crossed the reopen in flight, or a misbehaving client, and
    /// honoring it would only spend another self-contained IDR. The
    /// interval is measured from the last reopen of *either* kind
    /// (backpressure-driven or request-driven) for the same reason.
    pub const KEYFRAME_REQUEST_MIN_INTERVAL_US: u64 =
        crate::queue_circuit_breaker::defaults::TRIP_THRESHOLD_US;
}

/// What the server should do with a client `KeyframeRequest`
/// ([`VideoChannel::on_keyframe_request`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeRequestDecision {
    /// Honor it: `RESET_STREAM` the current Instance, then
    /// [`VideoChannel::prepare_reopen`] and open `generation + 1` with a
    /// self-contained IDR -- exactly the [`BackpressureDecision::ResetStream`]
    /// procedure.
    Reopen,
    /// Ignore it (log only); the current Instance continues untouched.
    Ignored(KeyframeRequestIgnored),
}

/// Why a `KeyframeRequest` was not honored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeRequestIgnored {
    /// Fewer than [`defaults::KEYFRAME_REQUEST_MIN_INTERVAL_US`] since the
    /// last reopen; `next_allowed_at_us` is when one would be honored.
    RateLimited { next_allowed_at_us: u64 },
    /// The Instance isn't `Streaming`/`Congested` (still configuring, or
    /// already closed and awaiting `prepare_reopen`), so there is nothing
    /// to reset -- the reopen already in progress will deliver an IDR.
    NotStreaming,
}

pub struct VideoChannel {
    channel_sm: ChannelSm,
    baseline: BaselineTracker,
    congestion: CongestionTracker,
    instance_sm: VideoInstanceSm,
    generation: u64,
    /// `now_us` of the most recent reopen decision of either kind
    /// (`ResetStream` from [`Self::on_feedback`], or `Reopen` from
    /// [`Self::on_keyframe_request`]); the reference point for
    /// `KEYFRAME_REQUEST_MIN_INTERVAL_US`.
    last_reopen_us: Option<u64>,
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
            last_reopen_us: None,
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
                self.last_reopen_us = Some(now_us);
            }
        }
        Ok(decision)
    }

    /// Decides whether to honor a client `KeyframeRequest` (spec 2.10,
    /// `feedback` stream; spec 4.5: the server MAY reopen at
    /// `generation + 1`). All three reasons are treated alike: each is
    /// the client saying it needs a fresh self-contained IDR to continue.
    ///
    /// On [`KeyframeRequestDecision::Reopen`] the SMs have already moved
    /// to `Closed(Reset)`/`Recovering`, and the caller MUST follow the
    /// same procedure as after a `ResetStream` decision: `RESET_STREAM`
    /// the old stream, [`Self::prepare_reopen`], open the new Instance,
    /// [`Self::mark_instance_streaming`].
    ///
    /// Requests are rate-limited to one honored reopen per
    /// [`defaults::KEYFRAME_REQUEST_MIN_INTERVAL_US`] (measured from the
    /// last reopen of either kind), so a client that keeps asking -- or
    /// several requests for the same episode -- cost at most one IDR per
    /// interval. This is the server-side counterpart of the client
    /// breaker's own re-arm cooldown; neither relies on the other.
    pub fn on_keyframe_request(
        &mut self,
        now_us: u64,
        request: KeyframeRequest,
    ) -> KeyframeRequestDecision {
        // The reason only matters for logging; keep it in the signature
        // so callers hand over the whole message.
        let _ = request.reason;
        if let Some(last) = self.last_reopen_us {
            let next_allowed_at_us =
                last.saturating_add(defaults::KEYFRAME_REQUEST_MIN_INTERVAL_US);
            if now_us < next_allowed_at_us {
                return KeyframeRequestDecision::Ignored(KeyframeRequestIgnored::RateLimited {
                    next_allowed_at_us,
                });
            }
        }
        if self.instance_sm.on_client_requested_reset().is_err() {
            return KeyframeRequestDecision::Ignored(KeyframeRequestIgnored::NotStreaming);
        }
        self.channel_sm.on_reset();
        self.last_reopen_us = Some(now_us);
        KeyframeRequestDecision::Reopen
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

    fn decode_error_request() -> KeyframeRequest {
        KeyframeRequest {
            reason: crate::messages::KeyframeReason::DecodeError,
        }
    }

    const MIN_INTERVAL: u64 = super::defaults::KEYFRAME_REQUEST_MIN_INTERVAL_US;

    #[test]
    fn keyframe_request_from_streaming_reopens_at_the_next_generation() {
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        // The server's own trackers see nothing wrong (no feedback at
        // all, let alone a congested one) -- the request still wins.
        assert_eq!(
            channel.on_keyframe_request(5_000_000, decode_error_request()),
            KeyframeRequestDecision::Reopen
        );
        assert_eq!(
            channel.instance_state(),
            InstanceState::Closed(CloseReason::Reset)
        );
        assert_eq!(channel.channel_state(), ChannelState::Recovering);

        assert_eq!(channel.prepare_reopen(), 1);
        channel.mark_instance_streaming().unwrap();
        assert_eq!(channel.channel_state(), ChannelState::Live);
        assert_eq!(channel.instance_state(), InstanceState::Streaming);
    }

    #[test]
    fn keyframe_request_from_congested_also_reopens() {
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        channel.on_feedback(0, 5_000, 0).unwrap();
        channel.on_feedback(100_000, 150_000, 0).unwrap();
        assert_eq!(channel.instance_state(), InstanceState::Congested);
        assert_eq!(
            channel.on_keyframe_request(200_000, decode_error_request()),
            KeyframeRequestDecision::Reopen
        );
        assert_eq!(channel.channel_state(), ChannelState::Recovering);
    }

    #[test]
    fn keyframe_requests_are_honored_at_most_once_per_min_interval() {
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        let t0 = 10_000_000;
        assert_eq!(
            channel.on_keyframe_request(t0, decode_error_request()),
            KeyframeRequestDecision::Reopen
        );
        channel.prepare_reopen();
        channel.mark_instance_streaming().unwrap();

        // A second request inside the interval is ignored, and the
        // Instance is untouched (still Streaming, no second reopen).
        assert_eq!(
            channel.on_keyframe_request(t0 + MIN_INTERVAL - 1, decode_error_request()),
            KeyframeRequestDecision::Ignored(KeyframeRequestIgnored::RateLimited {
                next_allowed_at_us: t0 + MIN_INTERVAL,
            })
        );
        assert_eq!(channel.instance_state(), InstanceState::Streaming);
        assert_eq!(channel.generation(), 1);

        // Exactly at the boundary it is honored again.
        assert_eq!(
            channel.on_keyframe_request(t0 + MIN_INTERVAL, decode_error_request()),
            KeyframeRequestDecision::Reopen
        );
        assert_eq!(channel.prepare_reopen(), 2);
    }

    #[test]
    fn rate_limit_check_comes_before_the_state_check() {
        // An ignored (rate-limited) request must not touch the SMs even
        // when they would otherwise allow the reset.
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        channel.on_keyframe_request(0, decode_error_request());
        channel.prepare_reopen();
        channel.mark_instance_streaming().unwrap();
        assert!(matches!(
            channel.on_keyframe_request(1, decode_error_request()),
            KeyframeRequestDecision::Ignored(KeyframeRequestIgnored::RateLimited { .. })
        ));
        assert_eq!(channel.channel_state(), ChannelState::Live);
    }

    #[test]
    fn a_backpressure_reset_also_starts_the_min_interval() {
        // A request that crossed a backpressure-driven reopen in flight
        // was made against the old generation; it must not cost a second
        // IDR right away.
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        let mut now_us = 0u64;
        channel.on_feedback(now_us, 5_000, 0).unwrap();
        now_us += 100_000;
        channel.on_feedback(now_us, 150_000, 0).unwrap();
        let mut last = BackpressureDecision::Continue;
        for _ in 0..3 {
            now_us += 100_000;
            last = channel.on_feedback(now_us, 400_000, 0).unwrap();
        }
        assert_eq!(last, BackpressureDecision::ResetStream);
        let reset_at = now_us;
        channel.prepare_reopen();
        channel.mark_instance_streaming().unwrap();

        assert_eq!(
            channel.on_keyframe_request(reset_at + 50_000, decode_error_request()),
            KeyframeRequestDecision::Ignored(KeyframeRequestIgnored::RateLimited {
                next_allowed_at_us: reset_at + MIN_INTERVAL,
            })
        );
        assert_eq!(
            channel.on_keyframe_request(reset_at + MIN_INTERVAL, decode_error_request()),
            KeyframeRequestDecision::Reopen
        );
    }

    #[test]
    fn keyframe_request_is_ignored_while_no_instance_is_streaming() {
        // Before the first Instance is up...
        let mut channel = VideoChannel::new(0);
        assert_eq!(
            channel.on_keyframe_request(0, decode_error_request()),
            KeyframeRequestDecision::Ignored(KeyframeRequestIgnored::NotStreaming)
        );
        assert_eq!(channel.channel_state(), ChannelState::Initializing);

        // ...and between a reset decision and the reopen (the caller is
        // mid-reopen; that reopen delivers the IDR).
        channel.mark_instance_streaming().unwrap();
        channel.on_keyframe_request(0, decode_error_request());
        assert_eq!(
            channel.on_keyframe_request(MIN_INTERVAL, decode_error_request()),
            KeyframeRequestDecision::Ignored(KeyframeRequestIgnored::NotStreaming)
        );
    }

    #[test]
    fn keyframe_request_reopen_keeps_the_baseline() {
        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        channel.on_feedback(0, 50_000, 0).unwrap();
        channel.on_keyframe_request(1_000, decode_error_request());
        channel.prepare_reopen();
        assert_eq!(channel.baseline_us(), Some(50_000));
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
