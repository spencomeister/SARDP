//! Client-side display buffer, enforcing spec 2.10's client-side MUST
//! rules:
//!
//! - Discard frames belonging to a generation older than the newest one
//!   seen.
//! - Don't update the displayed frame until `EncoderConfig` and the first
//!   IDR of a generation have been received (the caller already
//!   guarantees this by construction: nothing reaches
//!   [`ClientDisplay::submit_frame`] before `video_session`'s
//!   `VideoInstanceIntro` -- header+payload pair -- has been fully
//!   validated).
//!
//! M4 only ever sees one generation (single video Instance, no
//! backpressure-triggered reopen yet -- that's M5), so the stale-
//! generation path is unit-tested here rather than exercised by a live
//! two-generation stream.

use crate::messages::VideoFrameHeader;
use crate::timecode_frame::SyntheticFrame;

/// What happened when a frame was submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// The frame was newer than (or equal to a fresh) the current
    /// generation and is now the displayed frame.
    Displayed,
    /// The frame's generation was older than one already displayed; spec
    /// 2.10 MUST: discard it, keep showing the current frame.
    DiscardedStaleGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientDisplay {
    current_generation: Option<u64>,
    displayed_frame: Option<SyntheticFrame>,
    last_displayed_frame_id: Option<u64>,
}

impl Default for ClientDisplay {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientDisplay {
    pub fn new() -> Self {
        Self {
            current_generation: None,
            displayed_frame: None,
            last_displayed_frame_id: None,
        }
    }

    /// Submits a decoded frame for display. `header.generation` older
    /// than the generation already being displayed is discarded (spec
    /// 2.10); anything else (including the very first frame, or a newer
    /// generation) replaces the displayed frame.
    pub fn submit_frame(
        &mut self,
        header: &VideoFrameHeader,
        decoded: SyntheticFrame,
    ) -> SubmitOutcome {
        if let Some(current) = self.current_generation
            && header.generation < current
        {
            return SubmitOutcome::DiscardedStaleGeneration;
        }
        self.current_generation = Some(header.generation);
        self.displayed_frame = Some(decoded);
        self.last_displayed_frame_id = Some(header.frame_id);
        SubmitOutcome::Displayed
    }

    pub fn current_generation(&self) -> Option<u64> {
        self.current_generation
    }

    pub fn displayed_frame(&self) -> Option<&SyntheticFrame> {
        self.displayed_frame.as_ref()
    }

    pub fn last_displayed_frame_id(&self) -> Option<u64> {
        self.last_displayed_frame_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(generation: u64, frame_id: u64) -> VideoFrameHeader {
        VideoFrameHeader {
            generation,
            frame_id,
            config_id: 1,
            flags: 1,
            capture_ts: 0,
            encode_done_ts: 0,
            width: 2,
            height: 2,
            payload_len: 0,
        }
    }

    fn frame(tag: u8) -> SyntheticFrame {
        SyntheticFrame {
            width: 2,
            height: 2,
            rgb: vec![tag; 12],
        }
    }

    #[test]
    fn first_frame_is_always_displayed() {
        let mut display = ClientDisplay::new();
        let outcome = display.submit_frame(&header(0, 0), frame(1));
        assert_eq!(outcome, SubmitOutcome::Displayed);
        assert_eq!(display.current_generation(), Some(0));
        assert_eq!(display.displayed_frame(), Some(&frame(1)));
        assert_eq!(display.last_displayed_frame_id(), Some(0));
    }

    #[test]
    fn same_generation_later_frame_is_displayed() {
        let mut display = ClientDisplay::new();
        display.submit_frame(&header(0, 0), frame(1));
        let outcome = display.submit_frame(&header(0, 1), frame(2));
        assert_eq!(outcome, SubmitOutcome::Displayed);
        assert_eq!(display.displayed_frame(), Some(&frame(2)));
    }

    #[test]
    fn newer_generation_replaces_display() {
        let mut display = ClientDisplay::new();
        display.submit_frame(&header(0, 5), frame(1));
        let outcome = display.submit_frame(&header(1, 0), frame(2));
        assert_eq!(outcome, SubmitOutcome::Displayed);
        assert_eq!(display.current_generation(), Some(1));
        assert_eq!(display.displayed_frame(), Some(&frame(2)));
    }

    #[test]
    fn older_generation_is_discarded_and_display_unchanged() {
        let mut display = ClientDisplay::new();
        display.submit_frame(&header(2, 0), frame(1));
        let outcome = display.submit_frame(&header(1, 99), frame(2));
        assert_eq!(outcome, SubmitOutcome::DiscardedStaleGeneration);
        assert_eq!(display.current_generation(), Some(2));
        assert_eq!(display.displayed_frame(), Some(&frame(1)));
        assert_eq!(display.last_displayed_frame_id(), Some(0));
    }
}
