//! Client-side circuit breaker for the unbounded receive/decode queue
//! (KNOWN_ISSUES.md item 16, the 3W-1-d-3 follow-up).
//!
//! Background: the client never drops received frames (dropping P-frames
//! breaks the decoder's reference chain, DR-019), so its receive queue is
//! unbounded and congestion is left to the server-driven backpressure of
//! spec 2.10 (`client_queue_delay_us` -> DR-029 baseline-relative
//! thresholds -> `RESET_STREAM` + new generation). That loop needs a
//! feedback round trip plus three consecutive over-threshold samples to
//! fire; if the server is slow to react (or the feedback path itself is
//! what's congested), nothing bounds the client's memory in the meantime.
//!
//! [`ClientQueueCircuitBreaker`] is the emergency path for exactly that
//! case, kept deliberately separate from -- and much less sensitive than
//! -- the normal backpressure path so the two don't compete as a second
//! trigger (the concern that ruled out using `KeyframeRequest` for
//! routine congestion in the first place):
//!
//! - It watches the age of the oldest frame still waiting in the
//!   client's queue (a purely local measurement: received-at to now, on
//!   the client clock, so no TimeSync offset and no propagation-delay
//!   component -- which is why the thresholds here are absolute and not
//!   baseline-relative like DR-029's).
//! - When that age exceeds [`defaults::TRIP_THRESHOLD_US`] it trips and
//!   asks the caller to send one `KeyframeRequest{reason: DECODE_ERROR}`
//!   (spec 2.10, `feedback` stream); the server MAY answer by reopening
//!   at `generation + 1`, whereupon the client discards the old
//!   generation's backlog (spec 2.10 client MUST) and the queue drains.
//! - While tripped it stays silent, however long the queue stays high,
//!   until the age falls below [`defaults::REARM_THRESHOLD_US`]. Only
//!   then can it trip again. The gap between the two thresholds is the
//!   cooldown/hysteresis that keeps it from re-sending every sample.
//!
//! Like [`crate::backpressure`], this takes the measurement as an
//! explicit parameter instead of reading a clock, so callers and tests
//! drive it deterministically.

use crate::messages::{KeyframeReason, KeyframeRequest};

/// Default thresholds. Neither is a spec value (the spec leaves the
/// client-side queue unbounded and only offers `KeyframeRequest` as a
/// MAY); both are derived from the spec 2.10 / 4.7 backpressure numbers
/// in [`crate::backpressure::defaults`] so that the breaker sits strictly
/// behind the normal path.
pub mod defaults {
    use crate::backpressure::defaults::{
        HARD_THRESHOLD_CONSECUTIVE_COUNT, MAX_VIDEO_QUEUE_DURATION_DELTA_US,
        VIDEO_RATE_REDUCE_THRESHOLD_DELTA_US,
    };

    /// Queue age above which the breaker trips: 1 second.
    ///
    /// Why 1s: the server's own hard path resets an Instance once
    /// `client_queue_delay_us - baseline` has exceeded
    /// `MAX_VIDEO_QUEUE_DURATION_DELTA` (300ms) on
    /// `HARD_THRESHOLD_CONSECUTIVE_COUNT` (3) consecutive 100ms feedback
    /// intervals. With a stalled decoder the queue age grows at 1s/s, so
    /// the third violating sample is taken at roughly 300ms + 3 x 100ms
    /// = 600ms of queue age, and a healthy feedback loop has reset the
    /// stream (and the client has discarded the backlog) well before the
    /// age reaches 1s. The remaining ~400ms is several LAN/WAN round
    /// trips of slack; a queue that still reaches 1s means the normal
    /// path is not going to save us in time, which is precisely the case
    /// this breaker exists for. 1s is also 900ms (300ms x 3, the product
    /// of the two spec constants) rounded up to a value that is easy to
    /// reason about in logs, and at 60fps it bounds the backlog to ~60
    /// frames (single-digit MB at the 2560x1440 rates observed in
    /// 3W-1-d-3) before the emergency signal goes out.
    ///
    /// Expressed as the product of the spec constants plus one feedback
    /// interval of reporting latency so the derivation stays visible if
    /// the spec values change; today that is 900ms + 100ms = 1_000_000us.
    pub const TRIP_THRESHOLD_US: u64 = MAX_VIDEO_QUEUE_DURATION_DELTA_US as u64
        * HARD_THRESHOLD_CONSECUTIVE_COUNT as u64
        + TRANSPORT_FEEDBACK_INTERVAL_US;

    /// Queue age the breaker must fall below before it may trip again:
    /// 100ms, the same level (`VIDEO_RATE_REDUCE_THRESHOLD_DELTA`) at
    /// which the server considers an Instance healthy enough to leave
    /// Congested. A queue that has come back under this is by the
    /// server's own definition no longer congested, so a *second* trip
    /// after that is a new episode rather than the same one. Being a
    /// tenth of the trip threshold, an age oscillating around 1s can't
    /// chatter.
    pub const REARM_THRESHOLD_US: u64 = VIDEO_RATE_REDUCE_THRESHOLD_DELTA_US as u64;

    /// `TRANSPORT_FEEDBACK_INTERVAL` (spec 4.7, 100ms); duplicated here
    /// rather than imported because `feedback_session` keeps it as a
    /// function-local constant.
    const TRANSPORT_FEEDBACK_INTERVAL_US: u64 = 100_000;
}

// The default pair must leave a cooldown gap (see `with_thresholds`);
// checked at compile time so a future edit of either constant can't
// silently turn the breaker into a per-sample re-sender.
const _: () = assert!(defaults::REARM_THRESHOLD_US < defaults::TRIP_THRESHOLD_US);

/// See the module docs. One per video Channel on the client; construct a
/// fresh one on reconnection (the queue is empty then anyway).
#[derive(Debug, Clone)]
pub struct ClientQueueCircuitBreaker {
    trip_threshold_us: u64,
    rearm_threshold_us: u64,
    tripped: bool,
}

impl Default for ClientQueueCircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientQueueCircuitBreaker {
    /// A breaker with the [`defaults`] thresholds.
    pub fn new() -> Self {
        Self::with_thresholds(defaults::TRIP_THRESHOLD_US, defaults::REARM_THRESHOLD_US)
    }

    /// A breaker with explicit thresholds. `rearm_threshold_us` must be
    /// strictly below `trip_threshold_us`; without that gap there is no
    /// cooldown and the breaker would re-send on every sample above the
    /// threshold.
    pub fn with_thresholds(trip_threshold_us: u64, rearm_threshold_us: u64) -> Self {
        assert!(
            rearm_threshold_us < trip_threshold_us,
            "re-arm threshold ({rearm_threshold_us}us) must be below the trip threshold ({trip_threshold_us}us)"
        );
        Self {
            trip_threshold_us,
            rearm_threshold_us,
            tripped: false,
        }
    }

    /// Whether the breaker has tripped and not yet re-armed (i.e. a
    /// `KeyframeRequest` has gone out for the current episode).
    pub fn is_tripped(&self) -> bool {
        self.tripped
    }

    /// Feeds one measurement: the age (client clock, microseconds) of the
    /// oldest frame still waiting in the receive/decode queue, or 0 when
    /// the queue is empty. Call it whenever the queue changes (a frame is
    /// enqueued or dequeued) and, ideally, on a periodic tick as well so
    /// a queue that is simply not moving still gets observed.
    ///
    /// Returns `Some(request)` exactly once per episode -- on the sample
    /// that crosses the trip threshold while armed -- and the caller
    /// MUST send it on the `feedback` stream
    /// ([`crate::feedback_session::send_keyframe_request`]). Every other
    /// call returns `None`, including every call while the queue stays
    /// above the threshold after the trip.
    pub fn observe(&mut self, oldest_queued_age_us: u64) -> Option<KeyframeRequest> {
        if self.tripped {
            if oldest_queued_age_us < self.rearm_threshold_us {
                self.tripped = false;
            }
            return None;
        }
        if oldest_queued_age_us > self.trip_threshold_us {
            self.tripped = true;
            return Some(KeyframeRequest {
                reason: KeyframeReason::DecodeError,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRIP: u64 = defaults::TRIP_THRESHOLD_US;
    const REARM: u64 = defaults::REARM_THRESHOLD_US;

    fn decode_error() -> Option<KeyframeRequest> {
        Some(KeyframeRequest {
            reason: KeyframeReason::DecodeError,
        })
    }

    #[test]
    fn default_thresholds_sit_behind_the_server_side_backpressure_path() {
        // Trip: strictly more than 3 x 300ms (the server's hard path) so
        // the breaker can never fire before the server has had its three
        // consecutive samples; re-arm: the server's own Congested->
        // Streaming level; and a real gap between them.
        assert_eq!(TRIP, 1_000_000);
        assert!(
            TRIP > crate::backpressure::defaults::MAX_VIDEO_QUEUE_DURATION_DELTA_US as u64
                * crate::backpressure::defaults::HARD_THRESHOLD_CONSECUTIVE_COUNT as u64
        );
        assert_eq!(
            REARM,
            crate::backpressure::defaults::VIDEO_RATE_REDUCE_THRESHOLD_DELTA_US as u64
        );
    }

    #[test]
    fn starts_armed_and_silent_below_threshold() {
        let mut breaker = ClientQueueCircuitBreaker::new();
        assert!(!breaker.is_tripped());
        assert_eq!(breaker.observe(0), None);
        assert_eq!(breaker.observe(25), None); // the healthy ~25us of 3W-1-d-3
        assert_eq!(breaker.observe(REARM), None);
        assert_eq!(breaker.observe(TRIP), None); // at, not above
        assert!(!breaker.is_tripped());
    }

    #[test]
    fn crossing_the_threshold_sends_exactly_one_decode_error_request() {
        let mut breaker = ClientQueueCircuitBreaker::new();
        assert_eq!(breaker.observe(TRIP + 1), decode_error());
        assert!(breaker.is_tripped());
        // Same episode, queue still growing: nothing more goes out.
        assert_eq!(breaker.observe(TRIP + 2), None);
        assert_eq!(breaker.observe(TRIP * 10), None);
        assert!(breaker.is_tripped());
    }

    #[test]
    fn cooldown_holds_while_the_queue_is_between_rearm_and_trip() {
        let mut breaker = ClientQueueCircuitBreaker::new();
        assert_eq!(breaker.observe(TRIP + 1), decode_error());
        // Draining, but not yet "well below": still tripped, still silent
        // -- even when it climbs back over the trip threshold.
        assert_eq!(breaker.observe(TRIP - 1), None);
        assert_eq!(breaker.observe(REARM + 1), None);
        assert_eq!(breaker.observe(REARM), None); // must be strictly below
        assert!(breaker.is_tripped());
        assert_eq!(breaker.observe(TRIP + 1), None);
        assert!(breaker.is_tripped());
    }

    #[test]
    fn dropping_below_the_rearm_threshold_allows_a_new_trip() {
        let mut breaker = ClientQueueCircuitBreaker::new();
        assert_eq!(breaker.observe(TRIP + 1), decode_error());
        // The server reopened, the old generation's backlog was
        // discarded, the queue is empty.
        assert_eq!(breaker.observe(0), None);
        assert!(!breaker.is_tripped());
        // A fresh episode trips again.
        assert_eq!(breaker.observe(TRIP + 1), decode_error());
        assert!(breaker.is_tripped());
    }

    #[test]
    fn rearm_sample_itself_never_sends() {
        // Re-arming and tripping can't happen on the same sample: the
        // re-arm sample is by definition below the trip threshold.
        let mut breaker = ClientQueueCircuitBreaker::new();
        breaker.observe(TRIP + 1);
        assert_eq!(breaker.observe(REARM - 1), None);
        assert!(!breaker.is_tripped());
    }

    #[test]
    fn stalled_decoder_scenario_sends_once_per_generation() {
        // Queue age grows 1s/s from an empty queue (samples every 100ms),
        // the server reopens after the request, the client discards the
        // backlog, and then it happens all over again.
        let mut breaker = ClientQueueCircuitBreaker::new();
        let mut sent = 0;
        for episode in 0..2 {
            for step in 0..30u64 {
                if breaker.observe(step * 100_000).is_some() {
                    sent += 1;
                    assert_eq!(
                        step, 11,
                        "first sample strictly above 1s, episode {episode}"
                    );
                }
            }
            assert_eq!(breaker.observe(0), None); // new generation, queue empty
        }
        assert_eq!(sent, 2);
    }

    #[test]
    fn custom_thresholds_are_honored() {
        let mut breaker = ClientQueueCircuitBreaker::with_thresholds(500, 50);
        assert_eq!(breaker.observe(500), None);
        assert_eq!(breaker.observe(501), decode_error());
        assert_eq!(breaker.observe(50), None);
        assert!(breaker.is_tripped());
        assert_eq!(breaker.observe(49), None);
        assert!(!breaker.is_tripped());
    }

    #[test]
    #[should_panic(expected = "must be below the trip threshold")]
    fn rearm_threshold_must_leave_a_cooldown_gap() {
        let _ = ClientQueueCircuitBreaker::with_thresholds(500, 500);
    }
}
