//! TimeSync offset/RTT computation (spec 2.9).
//!
//! ```text
//! offset = ((t2 - t1) + (t3 - t4)) / 2
//! rtt    = (t4 - t1) - (t3 - t2)
//! ```
//!
//! `t1`/`t4` are the requester's local monotonic clock; `t2`/`t3` are the
//! responder's. `t4` never goes on the wire (spec 2.9): the requester
//! records it locally on receiving `TimeSyncResponse`.
//!
//! This module also drives the exchange itself over an already-open
//! `control` stream ([`crate::handshake::ControlChannel`]). Per the PoC
//! brief's M4 scope, the client is the requester (it needs `offset_us` to
//! compute `TransportFeedback.client_queue_delay_us`, spec 2.10); spec
//! 2.9 allows either side to initiate, but a one-shot PoC has no reason
//! for the server to.

use crate::clock;
use crate::handshake::ControlChannel;
use crate::messages::{self, TimeSyncRequest, TimeSyncResponse};
use crate::stream_kind::StreamKind;
use crate::stream_reader::{StreamReadError, write_envelope};

#[derive(Debug)]
pub enum TimeSyncError {
    Write(quinn::WriteError),
    Read(StreamReadError),
    Decode(ciborium::de::Error<std::io::Error>),
    UnexpectedType(u16),
}

/// How many rounds [`client_time_sync`] runs. One exchange is at the mercy
/// of whatever the two sides happen to be doing at that instant (the
/// first control-stream exchange after the handshake was measured at
/// 300-600ms RTT on a single machine, versus well under a millisecond for
/// the rounds after it); the round with the smallest RTT has the least
/// asymmetric queueing in it and so the most trustworthy offset -- the
/// same reasoning NTP clients apply.
pub const DEFAULT_TIME_SYNC_ROUNDS: usize = 8;

/// Client side: one `TimeSyncRequest`/`TimeSyncResponse` exchange. `t4`
/// (spec 2.9: never on the wire) is this function's own clock read on
/// receiving the response.
pub async fn client_time_sync_once(
    control: &mut ControlChannel,
) -> Result<TimeSyncResult, TimeSyncError> {
    let t1 = clock::now_us();
    let request = TimeSyncRequest { t1 };
    write_envelope(
        &mut control.send,
        messages::type_id::TIME_SYNC_REQUEST,
        &messages::encode(&request),
    )
    .await
    .map_err(TimeSyncError::Write)?;

    let (type_raw, payload) = control
        .reader
        .read_envelope(StreamKind::Control.max_envelope_length())
        .await
        .map_err(TimeSyncError::Read)?;
    let t4 = clock::now_us();
    if type_raw != messages::type_id::TIME_SYNC_RESPONSE {
        return Err(TimeSyncError::UnexpectedType(type_raw));
    }
    let response: TimeSyncResponse = messages::decode(&payload).map_err(TimeSyncError::Decode)?;
    Ok(compute(response.t1, response.t2, response.t3, t4))
}

/// Client side: `rounds` back-to-back exchanges (each waits for its
/// response before the next request), returned in order. The caller
/// picks with [`best_of`]. The responder must keep answering for the
/// whole burst ([`server_respond_time_sync_burst`]).
pub async fn client_time_sync_rounds(
    control: &mut ControlChannel,
    rounds: usize,
) -> Result<Vec<TimeSyncResult>, TimeSyncError> {
    let mut results = Vec::with_capacity(rounds);
    for _ in 0..rounds.max(1) {
        results.push(client_time_sync_once(control).await?);
    }
    Ok(results)
}

/// Client side: [`DEFAULT_TIME_SYNC_ROUNDS`] rounds, best sample.
pub async fn client_time_sync(
    control: &mut ControlChannel,
) -> Result<TimeSyncResult, TimeSyncError> {
    let rounds = client_time_sync_rounds(control, DEFAULT_TIME_SYNC_ROUNDS).await?;
    Ok(best_of(&rounds).expect("at least one round"))
}

/// The sample to trust: the one with the smallest RTT (ties: the later
/// one). `None` for an empty slice.
pub fn best_of(rounds: &[TimeSyncResult]) -> Option<TimeSyncResult> {
    rounds
        .iter()
        .copied()
        .fold(None, |best: Option<TimeSyncResult>, r| match best {
            Some(b) if b.rtt_us < r.rtt_us => Some(b),
            _ => Some(r),
        })
}

/// Server side: reads one `TimeSyncRequest` and replies with
/// `TimeSyncResponse`.
pub async fn server_respond_time_sync(control: &mut ControlChannel) -> Result<(), TimeSyncError> {
    let (type_raw, payload) = control
        .reader
        .read_envelope(StreamKind::Control.max_envelope_length())
        .await
        .map_err(TimeSyncError::Read)?;
    respond_to_time_sync_request(control, type_raw, &payload).await
}

/// Server side: answers an already-read `TimeSyncRequest` Envelope
/// (`t2` is taken now, so call this as soon as the Envelope is in hand --
/// spec 2.9 SHOULD: respond ahead of other control traffic).
pub async fn respond_to_time_sync_request(
    control: &mut ControlChannel,
    type_raw: u16,
    payload: &[u8],
) -> Result<(), TimeSyncError> {
    let response = answer_time_sync_request(type_raw, payload)?;
    write_envelope(
        &mut control.send,
        messages::type_id::TIME_SYNC_RESPONSE,
        &messages::encode(&response),
    )
    .await
    .map_err(TimeSyncError::Write)?;
    Ok(())
}

/// The `TimeSyncResponse` for a just-read `TimeSyncRequest` Envelope
/// (`t2`/`t3` are taken here), for a caller that writes to the control
/// stream itself (e.g. a server loop sharing its send half).
pub fn answer_time_sync_request(
    type_raw: u16,
    payload: &[u8],
) -> Result<TimeSyncResponse, TimeSyncError> {
    let t2 = clock::now_us();
    if type_raw != messages::type_id::TIME_SYNC_REQUEST {
        return Err(TimeSyncError::UnexpectedType(type_raw));
    }
    let request: TimeSyncRequest = messages::decode(payload).map_err(TimeSyncError::Decode)?;
    let t3 = clock::now_us();
    Ok(TimeSyncResponse {
        t1: request.t1,
        t2,
        t3,
    })
}

/// Server side, for a client doing [`client_time_sync_rounds`]: answers
/// the first request, then keeps answering as long as the next request
/// arrives within `follow_up_gap` of the previous answer. Returns how
/// many were answered. A client that only ever does one round costs the
/// server one `follow_up_gap` of waiting here, nothing more; anything
/// else the client sends inside the gap is a protocol error, since spec
/// 4.1's Authenticated phase has no other client-initiated control
/// message this early.
pub async fn server_respond_time_sync_burst(
    control: &mut ControlChannel,
    follow_up_gap: std::time::Duration,
) -> Result<usize, TimeSyncError> {
    server_respond_time_sync(control).await?;
    let mut answered = 1;
    loop {
        let next = tokio::time::timeout(
            follow_up_gap,
            control
                .reader
                .read_envelope(StreamKind::Control.max_envelope_length()),
        )
        .await;
        match next {
            Ok(Ok((type_raw, payload))) => {
                respond_to_time_sync_request(control, type_raw, &payload).await?;
                answered += 1;
            }
            Ok(Err(e)) => return Err(TimeSyncError::Read(e)),
            Err(_elapsed) => return Ok(answered),
        }
    }
}

/// The result of one TimeSync round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSyncResult {
    /// Responder's clock minus requester's clock, in microseconds. Adding
    /// this to a requester-clock timestamp converts it to the responder's
    /// clock (spec 2.10's `client_queue_delay_us` needs exactly this, to
    /// compare a client-local display time against a server-clock
    /// `capture_ts`).
    pub offset_us: i64,
    /// Round-trip time, in microseconds.
    pub rtt_us: u64,
}

/// Computes offset and RTT from the four TimeSync timestamps (spec 2.9).
/// All are microsecond values from independent monotonic clocks (not a
/// shared epoch), so differences are computed in `i64` to stay correct
/// regardless of which clock happens to read larger.
pub fn compute(t1: u64, t2: u64, t3: u64, t4: u64) -> TimeSyncResult {
    let (t1, t2, t3, t4) = (t1 as i64, t2 as i64, t3 as i64, t4 as i64);
    let offset_us = ((t2 - t1) + (t3 - t4)) / 2;
    let rtt_us = (t4 - t1) - (t3 - t2);
    TimeSyncResult {
        offset_us,
        // A negative computed RTT only happens with corrupted/adversarial
        // inputs (real clocks never produce t3 earlier relative to t2
        // than t4 is to t1 by more than measurement noise); clamp rather
        // than let a negative "duration" propagate.
        rtt_us: rtt_us.max(0) as u64,
    }
}

/// Converts a timestamp on the requester's local clock to the responder's
/// clock, using a previously computed `offset_us`.
pub fn to_responder_clock(local_ts: u64, offset_us: i64) -> u64 {
    (local_ts as i64 + offset_us).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_of_picks_the_smallest_rtt() {
        let rounds = [
            TimeSyncResult {
                offset_us: -300_000,
                rtt_us: 600_000,
            },
            TimeSyncResult {
                offset_us: -180_000,
                rtt_us: 400,
            },
            TimeSyncResult {
                offset_us: -180_010,
                rtt_us: 900,
            },
        ];
        assert_eq!(best_of(&rounds), Some(rounds[1]));
        assert_eq!(best_of(&[]), None);
        assert_eq!(best_of(&rounds[..1]), Some(rounds[0]));
    }

    #[test]
    fn zero_offset_zero_rtt_when_clocks_are_identical_and_instantaneous() {
        // t1=100 (request sent), t2=100 (received instantly), t3=100
        // (response sent instantly), t4=100 (received instantly).
        let result = compute(100, 100, 100, 100);
        assert_eq!(result.offset_us, 0);
        assert_eq!(result.rtt_us, 0);
    }

    #[test]
    fn detects_positive_clock_offset() {
        // Responder's clock reads 1000us ahead of requester's, network
        // delay negligible: t1=0, t2=1000, t3=1000, t4=0.
        let result = compute(0, 1_000, 1_000, 0);
        assert_eq!(result.offset_us, 1_000);
        assert_eq!(result.rtt_us, 0);
    }

    #[test]
    fn detects_negative_clock_offset() {
        // Responder's clock reads behind the requester's.
        let result = compute(1_000, 0, 0, 1_000);
        assert_eq!(result.offset_us, -1_000);
    }

    #[test]
    fn measures_symmetric_network_delay_as_rtt() {
        // 50us out, 50us back, no clock offset: t1=0, t2=50, t3=50, t4=100.
        let result = compute(0, 50, 50, 100);
        assert_eq!(result.offset_us, 0);
        assert_eq!(result.rtt_us, 100);
    }

    #[test]
    fn offset_and_delay_combine_correctly() {
        // Responder's clock is +500 ahead; 20us out, 20us back.
        // t1=0, t2=520, t3=520, t4=40.
        let result = compute(0, 520, 520, 40);
        assert_eq!(result.offset_us, 500);
        assert_eq!(result.rtt_us, 40);
    }

    #[test]
    fn to_responder_clock_applies_offset() {
        assert_eq!(to_responder_clock(1_000, 500), 1_500);
        assert_eq!(to_responder_clock(1_000, -500), 500);
    }

    #[test]
    fn to_responder_clock_saturates_at_zero() {
        assert_eq!(to_responder_clock(100, -1_000), 0);
    }
}
