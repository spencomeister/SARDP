//! VideoStream Instance State Machine (spec 4.3.2), through M5:
//! `Created -> Configuring -> Streaming -> Congested -> Closed(Reset)`.
//! The Channel layer (spec 4.3.1, which spans Instance reopens) lives in
//! [`crate::channel_sm`]; the backpressure decision logic that decides
//! *when* to call [`VideoInstanceSm::on_congested`] /
//! [`VideoInstanceSm::on_recovered`] / [`VideoInstanceSm::on_reset`] lives
//! in [`crate::backpressure`] -- this type only encodes which transitions
//! are legal from which state, not the threshold math.
//!
//! Per the spec 4.3.2 table, an Instance's `Configuring` state only
//! permits exactly one `VideoStreamGeneration` followed by exactly one
//! `EncoderConfig`, in that order, before the first IDR `VideoFrame`;
//! sending `VideoFrame` before that (or resending `VideoStreamGeneration`/
//! `EncoderConfig` afterward) is `PROTOCOL.UNEXPECTED_MESSAGE`.

use crate::reason_code::ReasonCode;

/// Instance-scoped timeouts (spec 4.7), applied by callers via
/// `tokio::time::timeout` (same convention as `connection_sm::defaults`).
pub mod defaults {
    use std::time::Duration;

    /// Time allowed in `Configuring` before the first IDR must be sent.
    pub const VIDEO_CONFIGURING_TIMEOUT: Duration = Duration::from_secs(3);
}

/// Why an Instance closed (spec 4.3.2: `Closed(Reset)` / `Closed(Failed)`
/// / `Closed(Normal)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Backpressure hard threshold exceeded; the Channel reopens with
    /// `generation + 1` (spec 2.10, DR-029).
    Reset,
    /// `VIDEO_CONFIGURING_TIMEOUT` exceeded before the first IDR.
    Failed,
    /// `SessionClose` / connection teardown.
    Normal,
}

/// Instance states through M5 (spec 4.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceState {
    Created,
    Configuring,
    Streaming,
    Congested,
    Closed(CloseReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolViolation {
    pub reason: ReasonCode,
}

fn unexpected() -> ProtocolViolation {
    ProtocolViolation {
        reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
    }
}

/// Drives one video stream Instance (one QUIC stream, one generation)
/// through the M3 subset of spec 4.3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoInstanceSm {
    state: InstanceState,
    sent_generation: bool,
    sent_encoder_config: bool,
}

impl Default for VideoInstanceSm {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoInstanceSm {
    pub fn new() -> Self {
        Self {
            state: InstanceState::Created,
            sent_generation: false,
            sent_encoder_config: false,
        }
    }

    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// `Created -> Configuring` on `StreamPrologue` (spec 4.3.2).
    pub fn on_prologue_sent(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            InstanceState::Created => {
                self.state = InstanceState::Configuring;
                Ok(())
            }
            _ => Err(unexpected()),
        }
    }

    /// Sends `VideoStreamGeneration`. MUST be the first message after the
    /// prologue, and exactly once per Instance.
    pub fn on_generation_sent(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            InstanceState::Configuring if !self.sent_generation => {
                self.sent_generation = true;
                Ok(())
            }
            _ => Err(unexpected()),
        }
    }

    /// Sends `EncoderConfig`. MUST follow `VideoStreamGeneration`, exactly
    /// once per Instance.
    pub fn on_encoder_config_sent(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            InstanceState::Configuring if self.sent_generation && !self.sent_encoder_config => {
                self.sent_encoder_config = true;
                Ok(())
            }
            _ => Err(unexpected()),
        }
    }

    /// Sends the first (self-contained IDR) `VideoFrame`.
    /// `Configuring -> Streaming` (spec 4.3.2).
    pub fn on_first_idr_sent(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            InstanceState::Configuring if self.sent_generation && self.sent_encoder_config => {
                self.state = InstanceState::Streaming;
                Ok(())
            }
            _ => Err(unexpected()),
        }
    }

    /// `Streaming -> Congested`, on a
    /// [`crate::backpressure::CongestionTracker::evaluate`] ->
    /// `EnterCongested` decision.
    pub fn on_congested(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            InstanceState::Streaming => {
                self.state = InstanceState::Congested;
                Ok(())
            }
            _ => Err(unexpected()),
        }
    }

    /// `Congested -> Streaming`, on an `ExitCongested` decision
    /// (hysteresis satisfied).
    pub fn on_recovered(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            InstanceState::Congested => {
                self.state = InstanceState::Streaming;
                Ok(())
            }
            _ => Err(unexpected()),
        }
    }

    /// `Congested -> Closed(Reset)`, on a `ResetStream` decision. The
    /// caller MUST then abandon this Instance's QUIC stream
    /// (`RESET_STREAM`) and open a new one with `generation + 1` (spec
    /// 2.10).
    pub fn on_reset(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            InstanceState::Congested => {
                self.state = InstanceState::Closed(CloseReason::Reset);
                Ok(())
            }
            _ => Err(unexpected()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_in_created() {
        assert_eq!(VideoInstanceSm::new().state(), InstanceState::Created);
    }

    #[test]
    fn prologue_transitions_to_configuring() {
        let mut sm = VideoInstanceSm::new();
        assert_eq!(sm.on_prologue_sent(), Ok(()));
        assert_eq!(sm.state(), InstanceState::Configuring);
    }

    #[test]
    fn generation_before_prologue_is_rejected() {
        let mut sm = VideoInstanceSm::new();
        assert!(sm.on_generation_sent().is_err());
    }

    #[test]
    fn encoder_config_before_generation_is_rejected() {
        let mut sm = VideoInstanceSm::new();
        sm.on_prologue_sent().unwrap();
        assert!(sm.on_encoder_config_sent().is_err());
    }

    #[test]
    fn idr_before_encoder_config_is_rejected() {
        let mut sm = VideoInstanceSm::new();
        sm.on_prologue_sent().unwrap();
        sm.on_generation_sent().unwrap();
        assert!(sm.on_first_idr_sent().is_err());
    }

    #[test]
    fn happy_path_reaches_streaming() {
        let mut sm = VideoInstanceSm::new();
        sm.on_prologue_sent().unwrap();
        sm.on_generation_sent().unwrap();
        sm.on_encoder_config_sent().unwrap();
        assert_eq!(sm.on_first_idr_sent(), Ok(()));
        assert_eq!(sm.state(), InstanceState::Streaming);
    }

    #[test]
    fn resending_generation_is_rejected() {
        let mut sm = VideoInstanceSm::new();
        sm.on_prologue_sent().unwrap();
        sm.on_generation_sent().unwrap();
        assert!(sm.on_generation_sent().is_err());
    }

    #[test]
    fn resending_encoder_config_is_rejected() {
        let mut sm = VideoInstanceSm::new();
        sm.on_prologue_sent().unwrap();
        sm.on_generation_sent().unwrap();
        sm.on_encoder_config_sent().unwrap();
        assert!(sm.on_encoder_config_sent().is_err());
    }

    #[test]
    fn congested_then_reset_reaches_closed_reset() {
        let mut sm = VideoInstanceSm::new();
        sm.on_prologue_sent().unwrap();
        sm.on_generation_sent().unwrap();
        sm.on_encoder_config_sent().unwrap();
        sm.on_first_idr_sent().unwrap();
        assert_eq!(sm.on_congested(), Ok(()));
        assert_eq!(sm.state(), InstanceState::Congested);
        assert_eq!(sm.on_reset(), Ok(()));
        assert_eq!(sm.state(), InstanceState::Closed(CloseReason::Reset));
    }

    #[test]
    fn congested_can_recover_back_to_streaming() {
        let mut sm = VideoInstanceSm::new();
        sm.on_prologue_sent().unwrap();
        sm.on_generation_sent().unwrap();
        sm.on_encoder_config_sent().unwrap();
        sm.on_first_idr_sent().unwrap();
        sm.on_congested().unwrap();
        assert_eq!(sm.on_recovered(), Ok(()));
        assert_eq!(sm.state(), InstanceState::Streaming);
    }

    #[test]
    fn congested_before_streaming_is_rejected() {
        let mut sm = VideoInstanceSm::new();
        assert!(sm.on_congested().is_err());
    }

    #[test]
    fn reset_before_congested_is_rejected() {
        let mut sm = VideoInstanceSm::new();
        sm.on_prologue_sent().unwrap();
        sm.on_generation_sent().unwrap();
        sm.on_encoder_config_sent().unwrap();
        sm.on_first_idr_sent().unwrap();
        assert!(sm.on_reset().is_err());
        assert_eq!(sm.state(), InstanceState::Streaming);
    }

    #[test]
    fn recovered_before_congested_is_rejected() {
        let mut sm = VideoInstanceSm::new();
        sm.on_prologue_sent().unwrap();
        sm.on_generation_sent().unwrap();
        sm.on_encoder_config_sent().unwrap();
        sm.on_first_idr_sent().unwrap();
        assert!(sm.on_recovered().is_err());
    }

    #[test]
    fn encoder_config_and_generation_are_unexpected_message_reason() {
        let mut sm = VideoInstanceSm::new();
        assert_eq!(
            sm.on_encoder_config_sent(),
            Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE
            })
        );
    }
}
