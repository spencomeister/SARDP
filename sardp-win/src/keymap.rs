//! USB HID keyboard usage (page 0x07, what spec 2.12's `KeyEvent.scancode`
//! carries) <-> Windows scan code (PS/2 set 1 make code + the `E0`
//! extended flag, what `WM_KEYDOWN`'s `lParam` reports and what
//! `SendInput(KEYEVENTF_SCANCODE)` takes).
//!
//! Layout-independent on both ends: HID usages name physical key
//! positions, and Windows scan codes do too. The character a key produces
//! is never derived from this table (spec 2.12: characters come from
//! `TextInput`).

/// `(hid_usage, windows_scancode, extended)`.
const TABLE: &[(u32, u16, bool)] = &[
    (0x04, 0x1E, false), // a
    (0x05, 0x30, false), // b
    (0x06, 0x2E, false), // c
    (0x07, 0x20, false), // d
    (0x08, 0x12, false), // e
    (0x09, 0x21, false), // f
    (0x0A, 0x22, false), // g
    (0x0B, 0x23, false), // h
    (0x0C, 0x17, false), // i
    (0x0D, 0x24, false), // j
    (0x0E, 0x25, false), // k
    (0x0F, 0x26, false), // l
    (0x10, 0x32, false), // m
    (0x11, 0x31, false), // n
    (0x12, 0x18, false), // o
    (0x13, 0x19, false), // p
    (0x14, 0x10, false), // q
    (0x15, 0x13, false), // r
    (0x16, 0x1F, false), // s
    (0x17, 0x14, false), // t
    (0x18, 0x16, false), // u
    (0x19, 0x2F, false), // v
    (0x1A, 0x11, false), // w
    (0x1B, 0x2D, false), // x
    (0x1C, 0x15, false), // y
    (0x1D, 0x2C, false), // z
    (0x1E, 0x02, false), // 1
    (0x1F, 0x03, false), // 2
    (0x20, 0x04, false), // 3
    (0x21, 0x05, false), // 4
    (0x22, 0x06, false), // 5
    (0x23, 0x07, false), // 6
    (0x24, 0x08, false), // 7
    (0x25, 0x09, false), // 8
    (0x26, 0x0A, false), // 9
    (0x27, 0x0B, false), // 0
    (0x28, 0x1C, false), // Enter
    (0x29, 0x01, false), // Escape
    (0x2A, 0x0E, false), // Backspace
    (0x2B, 0x0F, false), // Tab
    (0x2C, 0x39, false), // Space
    (0x2D, 0x0C, false), // - _
    (0x2E, 0x0D, false), // = +
    (0x2F, 0x1A, false), // [ {
    (0x30, 0x1B, false), // ] }
    (0x31, 0x2B, false), // \ | (US)
    (0x33, 0x27, false), // ; :
    (0x34, 0x28, false), // ' "
    (0x35, 0x29, false), // ` ~ (JIS: 半角/全角)
    (0x36, 0x33, false), // , <
    (0x37, 0x34, false), // . >
    (0x38, 0x35, false), // / ?
    (0x39, 0x3A, false), // Caps Lock
    (0x3A, 0x3B, false), // F1
    (0x3B, 0x3C, false), // F2
    (0x3C, 0x3D, false), // F3
    (0x3D, 0x3E, false), // F4
    (0x3E, 0x3F, false), // F5
    (0x3F, 0x40, false), // F6
    (0x40, 0x41, false), // F7
    (0x41, 0x42, false), // F8
    (0x42, 0x43, false), // F9
    (0x43, 0x44, false), // F10
    (0x44, 0x57, false), // F11
    (0x45, 0x58, false), // F12
    (0x46, 0x37, true),  // Print Screen
    (0x47, 0x46, false), // Scroll Lock
    (0x49, 0x52, true),  // Insert
    (0x4A, 0x47, true),  // Home
    (0x4B, 0x49, true),  // Page Up
    (0x4C, 0x53, true),  // Delete
    (0x4D, 0x4F, true),  // End
    (0x4E, 0x51, true),  // Page Down
    (0x4F, 0x4D, true),  // Right
    (0x50, 0x4B, true),  // Left
    (0x51, 0x50, true),  // Down
    (0x52, 0x48, true),  // Up
    (0x53, 0x45, false), // Num Lock (Pause reports the same code in lParam; see docs)
    (0x54, 0x35, true),  // Keypad /
    (0x55, 0x37, false), // Keypad *
    (0x56, 0x4A, false), // Keypad -
    (0x57, 0x4E, false), // Keypad +
    (0x58, 0x1C, true),  // Keypad Enter
    (0x59, 0x4F, false), // Keypad 1
    (0x5A, 0x50, false), // Keypad 2
    (0x5B, 0x51, false), // Keypad 3
    (0x5C, 0x4B, false), // Keypad 4
    (0x5D, 0x4C, false), // Keypad 5
    (0x5E, 0x4D, false), // Keypad 6
    (0x5F, 0x47, false), // Keypad 7
    (0x60, 0x48, false), // Keypad 8
    (0x61, 0x49, false), // Keypad 9
    (0x62, 0x52, false), // Keypad 0
    (0x63, 0x53, false), // Keypad .
    (0x64, 0x56, false), // Non-US \ | (ISO)
    (0x65, 0x5D, true),  // Application (menu)
    (0x67, 0x59, false), // Keypad =
    (0x68, 0x64, false), // F13
    (0x69, 0x65, false), // F14
    (0x6A, 0x66, false), // F15
    (0x6B, 0x67, false), // F16
    (0x6C, 0x68, false), // F17
    (0x6D, 0x69, false), // F18
    (0x6E, 0x6A, false), // F19
    (0x6F, 0x6B, false), // F20
    (0x70, 0x6C, false), // F21
    (0x71, 0x6D, false), // F22
    (0x72, 0x6E, false), // F23
    (0x73, 0x76, false), // F24
    (0x87, 0x73, false), // International1 (JIS \ _ ろ)
    (0x88, 0x70, false), // International2 (JIS カタカナ/ひらがな)
    (0x89, 0x7D, false), // International3 (JIS ¥ |)
    (0x8A, 0x79, false), // International4 (JIS 変換)
    (0x8B, 0x7B, false), // International5 (JIS 無変換)
    (0x90, 0x72, false), // LANG1 (Hangul/English)
    (0x91, 0x71, false), // LANG2 (Hanja)
    (0xE0, 0x1D, false), // Left Ctrl
    (0xE1, 0x2A, false), // Left Shift
    (0xE2, 0x38, false), // Left Alt
    (0xE3, 0x5B, true),  // Left GUI (Windows)
    (0xE4, 0x1D, true),  // Right Ctrl
    (0xE5, 0x36, false), // Right Shift
    (0xE6, 0x38, true),  // Right Alt
    (0xE7, 0x5C, true),  // Right GUI (Windows)
];

/// Windows scan code (+ extended flag) for a HID usage; `None` for keys
/// this table doesn't cover (media keys, Pause, ...).
pub fn hid_to_scancode(hid_usage: u32) -> Option<(u16, bool)> {
    TABLE
        .iter()
        .find(|(hid, _, _)| *hid == hid_usage)
        .map(|(_, sc, ext)| (*sc, *ext))
}

/// HID usage for a Windows scan code as reported in `WM_KEYDOWN`'s
/// `lParam` (bits 16-23 = scan code, bit 24 = extended).
pub fn scancode_to_hid(scancode: u16, extended: bool) -> Option<u32> {
    TABLE
        .iter()
        .find(|(_, sc, ext)| *sc == scancode && *ext == extended)
        .map(|(hid, _, _)| *hid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_has_no_duplicates_on_either_side() {
        let mut hids = HashSet::new();
        let mut scancodes = HashSet::new();
        for (hid, sc, ext) in TABLE {
            assert!(hids.insert(*hid), "duplicate HID usage {hid:#x}");
            assert!(
                scancodes.insert((*sc, *ext)),
                "duplicate scan code {sc:#x} ext={ext}"
            );
        }
    }

    #[test]
    fn round_trips_every_entry() {
        for (hid, sc, ext) in TABLE {
            assert_eq!(hid_to_scancode(*hid), Some((*sc, *ext)));
            assert_eq!(scancode_to_hid(*sc, *ext), Some(*hid));
        }
    }

    #[test]
    fn spot_checks() {
        assert_eq!(hid_to_scancode(0x04), Some((0x1E, false))); // a
        assert_eq!(hid_to_scancode(0x28), Some((0x1C, false))); // Enter
        assert_eq!(hid_to_scancode(0x58), Some((0x1C, true))); // keypad Enter
        assert_eq!(hid_to_scancode(0x4F), Some((0x4D, true))); // Right arrow
        assert_eq!(scancode_to_hid(0x4D, false), Some(0x5E)); // keypad 6, same code unextended
        assert_eq!(hid_to_scancode(0xE3), Some((0x5B, true))); // Left Windows
        assert_eq!(hid_to_scancode(0x48), None); // Pause: not mapped
        assert_eq!(scancode_to_hid(0x00, false), None);
    }
}
