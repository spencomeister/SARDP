//! `PermissionSet` bitflags (spec 2.5), bit positions per DR-033.

/// `PermissionSet.granted_permissions` bit positions (spec 2.5, DR-033).
/// Bits 10-31 are reserved for future flags.
pub mod bit {
    pub const VIEW: u32 = 1 << 0;
    pub const INPUT_KEYBOARD: u32 = 1 << 1;
    pub const INPUT_MOUSE: u32 = 1 << 2;
    pub const CLIP_READ: u32 = 1 << 3;
    pub const CLIP_WRITE: u32 = 1 << 4;
    pub const FILE_UP: u32 = 1 << 5;
    pub const FILE_DOWN: u32 = 1 << 6;
    pub const AUDIO_PLAYBACK: u32 = 1 << 7;
    pub const AUDIO_CAPTURE: u32 = 1 << 8;
    pub const ADMIN: u32 = 1 << 9;
}

#[cfg(test)]
mod tests {
    use super::bit;

    #[test]
    fn bit_positions_match_dr_033() {
        assert_eq!(bit::VIEW, 0b1);
        assert_eq!(bit::INPUT_KEYBOARD, 0b10);
        assert_eq!(bit::INPUT_MOUSE, 0b100);
        assert_eq!(bit::CLIP_READ, 1 << 3);
        assert_eq!(bit::CLIP_WRITE, 1 << 4);
        assert_eq!(bit::FILE_UP, 1 << 5);
        assert_eq!(bit::FILE_DOWN, 1 << 6);
        assert_eq!(bit::AUDIO_PLAYBACK, 1 << 7);
        assert_eq!(bit::AUDIO_CAPTURE, 1 << 8);
        assert_eq!(bit::ADMIN, 1 << 9);
    }

    #[test]
    fn all_bits_are_distinct() {
        let all = [
            bit::VIEW,
            bit::INPUT_KEYBOARD,
            bit::INPUT_MOUSE,
            bit::CLIP_READ,
            bit::CLIP_WRITE,
            bit::FILE_UP,
            bit::FILE_DOWN,
            bit::AUDIO_PLAYBACK,
            bit::AUDIO_CAPTURE,
            bit::ADMIN,
        ];
        let combined = all.iter().fold(0u32, |acc, b| acc | b);
        let popcount: u32 = all.iter().map(|b| b.count_ones()).sum();
        assert_eq!(combined.count_ones(), popcount);
    }
}
