//! Synthetic frame source with an embedded timecode (Part 8's measurement
//! harness approach: "タイムコード埋め込み合成画像"). Used as the source
//! image handed to the H.264 encoder in place of real OS screen capture
//! (out of scope for the PoC, spec brief section "省略してよいこと").
//!
//! The timecode is embedded as a row of fixed-size black/white blocks
//! (one block per bit) at a fixed position, rather than rendered text --
//! this needs no font/rasterization dependency and is trivial to decode
//! back out programmatically, which the Part 8 E2E latency harness (M6)
//! will need to do after client-side decode.
//!
//! Surviving H.264's lossy compression intact is *not* verified here --
//! that is M6's concern once real capture_ts round-tripping through the
//! encoder/decoder pipeline is being measured, and will likely need
//! encoder tuning (e.g. a QP map favoring this region) to get a clean
//! bit read back after compression.

/// Pixel width/height of each timecode bit's block.
pub const BIT_BLOCK_SIZE: u32 = 8;
/// Number of bits embedded (one `u64` timecode).
pub const TIMECODE_BITS: u32 = 64;

const WHITE: [u8; 3] = [255, 255, 255];
const BLACK: [u8; 3] = [0, 0, 0];

/// A synthetic RGB24 frame (row-major, 3 bytes per pixel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticFrame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl SyntheticFrame {
    fn set_pixel(&mut self, x: u32, y: u32, color: [u8; 3]) {
        let idx = ((y * self.width + x) * 3) as usize;
        self.rgb[idx..idx + 3].copy_from_slice(&color);
    }

    fn get_pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let idx = ((y * self.width + x) * 3) as usize;
        [self.rgb[idx], self.rgb[idx + 1], self.rgb[idx + 2]]
    }
}

/// Generates a `width`x`height` RGB24 frame of `background`, with
/// `timecode_us` embedded as a row of black/white bit-blocks in the
/// top-left corner (bit 0 leftmost).
///
/// Panics if `width`/`height` are too small to hold the timecode block
/// row -- a programming error (fixed encoder config), not attacker input.
pub fn generate_timecode_frame(
    width: u32,
    height: u32,
    timecode_us: u64,
    background: [u8; 3],
) -> SyntheticFrame {
    let required_width = TIMECODE_BITS * BIT_BLOCK_SIZE;
    assert!(
        width >= required_width && height >= BIT_BLOCK_SIZE,
        "frame {width}x{height} too small to hold a {TIMECODE_BITS}-bit timecode row ({required_width}x{BIT_BLOCK_SIZE} minimum)"
    );

    let mut frame = SyntheticFrame {
        width,
        height,
        rgb: vec![0u8; (width * height * 3) as usize],
    };
    for y in 0..height {
        for x in 0..width {
            frame.set_pixel(x, y, background);
        }
    }
    for bit in 0..TIMECODE_BITS {
        let color = if (timecode_us >> bit) & 1 == 1 {
            WHITE
        } else {
            BLACK
        };
        let x0 = bit * BIT_BLOCK_SIZE;
        for y in 0..BIT_BLOCK_SIZE {
            for x in x0..x0 + BIT_BLOCK_SIZE {
                frame.set_pixel(x, y, color);
            }
        }
    }
    frame
}

/// Reads the timecode back out of a frame produced by
/// [`generate_timecode_frame`] (or a lossily-reproduced version of one),
/// using the block's center pixel and a brightness threshold per bit.
pub fn extract_timecode(frame: &SyntheticFrame) -> u64 {
    let mut value = 0u64;
    for bit in 0..TIMECODE_BITS {
        let cx = bit * BIT_BLOCK_SIZE + BIT_BLOCK_SIZE / 2;
        let cy = BIT_BLOCK_SIZE / 2;
        let [r, g, b] = frame.get_pixel(cx, cy);
        let brightness = u32::from(r) + u32::from(g) + u32::from(b);
        if brightness > 3 * 128 {
            value |= 1 << bit;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_arbitrary_timecode() {
        for timecode in [0u64, 1, u64::MAX, 0x0123_4567_89AB_CDEF, 1_234_567_890] {
            let frame = generate_timecode_frame(640, 64, timecode, [64, 64, 64]);
            assert_eq!(extract_timecode(&frame), timecode);
        }
    }

    #[test]
    fn frame_has_expected_byte_length() {
        let frame = generate_timecode_frame(640, 64, 0, [0, 0, 0]);
        assert_eq!(frame.rgb.len(), 640 * 64 * 3);
    }

    #[test]
    fn background_is_visible_outside_the_timecode_row() {
        let background = [200, 100, 50];
        let frame = generate_timecode_frame(640, 64, 0, background);
        assert_eq!(frame.get_pixel(0, 63), background);
    }

    #[test]
    fn zero_bit_renders_black_and_one_bit_renders_white() {
        // timecode = 0b10 -> bit0=0 (black), bit1=1 (white)
        let frame = generate_timecode_frame(640, 64, 0b10, [128, 128, 128]);
        assert_eq!(frame.get_pixel(BIT_BLOCK_SIZE / 2, 0), BLACK);
        assert_eq!(
            frame.get_pixel(BIT_BLOCK_SIZE + BIT_BLOCK_SIZE / 2, 0),
            WHITE
        );
    }

    #[test]
    #[should_panic]
    fn panics_if_frame_too_narrow_for_all_bits() {
        generate_timecode_frame(100, 64, 0, [0, 0, 0]);
    }
}
