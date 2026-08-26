//! Stream kinds and their properties, per spec 2.2.1 (StreamPrologue.kind)
//! and the length-limit table in 2.1.1.

/// The `kind` byte carried in a `StreamPrologue` (spec 2.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Control,
    Input,
    Video,
    Feedback,
    Clipboard,
    File,
    AudioPlayback,
    AudioCapture,
}

impl StreamKind {
    /// Maps a `StreamPrologue.kind` byte to a [`StreamKind`], per the table
    /// in spec 2.2.1. Returns `None` for any value not in that table, which
    /// callers MUST treat as `PROTOCOL.UNKNOWN_STREAM_KIND` (spec 2.2, 4.2).
    pub fn from_byte(kind: u8) -> Option<Self> {
        match kind {
            0x01 => Some(Self::Control),
            0x02 => Some(Self::Input),
            0x03 => Some(Self::Video),
            0x04 => Some(Self::Feedback),
            0x05 => Some(Self::Clipboard),
            0x06 => Some(Self::File),
            0x07 => Some(Self::AudioPlayback),
            0x08 => Some(Self::AudioCapture),
            _ => None,
        }
    }

    /// The raw `kind` byte, as used on the wire.
    pub fn to_byte(self) -> u8 {
        match self {
            Self::Control => 0x01,
            Self::Input => 0x02,
            Self::Video => 0x03,
            Self::Feedback => 0x04,
            Self::Clipboard => 0x05,
            Self::File => 0x06,
            Self::AudioPlayback => 0x07,
            Self::AudioCapture => 0x08,
        }
    }

    /// The maximum permitted `Envelope.length` (payload length) on a stream
    /// of this kind, per the table in spec 2.1.1.
    pub fn max_envelope_length(self) -> u64 {
        match self {
            Self::Control => 64 * 1024,
            Self::Input => 1024,
            Self::Video => 8 * 1024 * 1024,
            Self::Feedback => 4 * 1024,
            Self::AudioPlayback | Self::AudioCapture => 64 * 1024,
            Self::Clipboard => 16 * 1024 * 1024,
            Self::File => 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_known_kinds() {
        for byte in [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08] {
            let kind = StreamKind::from_byte(byte).expect("known kind");
            assert_eq!(kind.to_byte(), byte);
        }
    }

    #[test]
    fn unknown_kind_byte_is_none() {
        for byte in [0x00, 0x09, 0x7F, 0xFF] {
            assert_eq!(StreamKind::from_byte(byte), None);
        }
    }

    #[test]
    fn length_limits_match_spec_table() {
        assert_eq!(StreamKind::Control.max_envelope_length(), 65_536);
        assert_eq!(StreamKind::Input.max_envelope_length(), 1_024);
        assert_eq!(StreamKind::Video.max_envelope_length(), 8 * 1024 * 1024);
        assert_eq!(StreamKind::Feedback.max_envelope_length(), 4_096);
        assert_eq!(StreamKind::AudioPlayback.max_envelope_length(), 65_536);
        assert_eq!(StreamKind::AudioCapture.max_envelope_length(), 65_536);
        assert_eq!(
            StreamKind::Clipboard.max_envelope_length(),
            16 * 1024 * 1024
        );
        assert_eq!(StreamKind::File.max_envelope_length(), 1024 * 1024);
    }
}
