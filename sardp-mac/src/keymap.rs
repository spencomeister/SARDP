//! USB HID keyboard usage (page 0x07, what spec 2.12's `KeyEvent.scancode`
//! carries) <-> macOS virtual key code (`kVK_*` from Carbon's `Events.h`,
//! what `CGEvent(keyboardEventSource:virtualKey:keyDown:)` takes).
//!
//! Layout-independent on both ends, exactly as the Windows table
//! (`sardp_win::keymap`) is: HID usages name physical key positions, and
//! so do macOS virtual key codes -- `kVK_ANSI_A` is "the key where A sits
//! on a US keyboard", not "the key that produces 'a'". The character a key
//! produces is never derived from this table (spec 2.12: characters come
//! from `TextInput`).
//!
//! macOS virtual key codes look arbitrary because they are: they are the
//! 1984 Macintosh keyboard's wiring order, which is why `kVK_ANSI_S` is
//! 0x01 and `kVK_ANSI_B` is 0x0B.

/// `(hid_usage, macos_virtual_key)`.
const TABLE: &[(u32, u16)] = &[
    (0x04, 0x00), // a
    (0x05, 0x0B), // b
    (0x06, 0x08), // c
    (0x07, 0x02), // d
    (0x08, 0x0E), // e
    (0x09, 0x03), // f
    (0x0A, 0x05), // g
    (0x0B, 0x04), // h
    (0x0C, 0x22), // i
    (0x0D, 0x26), // j
    (0x0E, 0x28), // k
    (0x0F, 0x25), // l
    (0x10, 0x2E), // m
    (0x11, 0x2D), // n
    (0x12, 0x1F), // o
    (0x13, 0x23), // p
    (0x14, 0x0C), // q
    (0x15, 0x0F), // r
    (0x16, 0x01), // s
    (0x17, 0x11), // t
    (0x18, 0x20), // u
    (0x19, 0x09), // v
    (0x1A, 0x0D), // w
    (0x1B, 0x07), // x
    (0x1C, 0x10), // y
    (0x1D, 0x06), // z
    (0x1E, 0x12), // 1
    (0x1F, 0x13), // 2
    (0x20, 0x14), // 3
    (0x21, 0x15), // 4
    (0x22, 0x17), // 5
    (0x23, 0x16), // 6
    (0x24, 0x1A), // 7
    (0x25, 0x1C), // 8
    (0x26, 0x19), // 9
    (0x27, 0x1D), // 0
    (0x28, 0x24), // Return
    (0x29, 0x35), // Escape
    (0x2A, 0x33), // Backspace (kVK_Delete)
    (0x2B, 0x30), // Tab
    (0x2C, 0x31), // Space
    (0x2D, 0x1B), // - _
    (0x2E, 0x18), // = +
    (0x2F, 0x21), // [ {
    (0x30, 0x1E), // ] }
    (0x31, 0x2A), // \ | (US)
    (0x33, 0x29), // ; :
    (0x34, 0x27), // ' "
    (0x35, 0x32), // ` ~
    (0x36, 0x2B), // , <
    (0x37, 0x2F), // . >
    (0x38, 0x2C), // / ?
    (0x39, 0x39), // Caps Lock
    (0x3A, 0x7A), // F1
    (0x3B, 0x78), // F2
    (0x3C, 0x63), // F3
    (0x3D, 0x76), // F4
    (0x3E, 0x60), // F5
    (0x3F, 0x61), // F6
    (0x40, 0x62), // F7
    (0x41, 0x64), // F8
    (0x42, 0x65), // F9
    (0x43, 0x6D), // F10
    (0x44, 0x67), // F11
    (0x45, 0x6F), // F12
    (0x49, 0x72), // Insert -> kVK_Help (the key in that position on a Mac)
    (0x4A, 0x73), // Home
    (0x4B, 0x74), // Page Up
    (0x4C, 0x75), // Delete (forward)
    (0x4D, 0x77), // End
    (0x4E, 0x79), // Page Down
    (0x4F, 0x7C), // Right
    (0x50, 0x7B), // Left
    (0x51, 0x7D), // Down
    (0x52, 0x7E), // Up
    (0x53, 0x47), // Num Lock -> kVK_ANSI_KeypadClear
    (0x54, 0x4B), // Keypad /
    (0x55, 0x43), // Keypad *
    (0x56, 0x4E), // Keypad -
    (0x57, 0x45), // Keypad +
    (0x58, 0x4C), // Keypad Enter
    (0x59, 0x53), // Keypad 1
    (0x5A, 0x54), // Keypad 2
    (0x5B, 0x55), // Keypad 3
    (0x5C, 0x56), // Keypad 4
    (0x5D, 0x57), // Keypad 5
    (0x5E, 0x58), // Keypad 6
    (0x5F, 0x59), // Keypad 7
    (0x60, 0x5B), // Keypad 8
    (0x61, 0x5C), // Keypad 9
    (0x62, 0x52), // Keypad 0
    (0x63, 0x41), // Keypad .
    (0x64, 0x0A), // Non-US \ | (ISO) -> kVK_ISO_Section
    (0x67, 0x51), // Keypad =
    (0x68, 0x69), // F13
    (0x69, 0x6B), // F14
    (0x6A, 0x71), // F15
    (0x6B, 0x6A), // F16
    (0x6C, 0x40), // F17
    (0x6D, 0x4F), // F18
    (0x6E, 0x50), // F19
    (0x6F, 0x5A), // F20
    (0x87, 0x5E), // International1 (JIS \ _ ろ) -> kVK_JIS_Underscore
    (0x89, 0x5D), // International3 (JIS ¥ |) -> kVK_JIS_Yen
    (0x90, 0x68), // LANG1 (かな) -> kVK_JIS_Kana
    (0x91, 0x66), // LANG2 (英数) -> kVK_JIS_Eisu
    (0xE0, 0x3B), // Left Control
    (0xE1, 0x38), // Left Shift
    (0xE2, 0x3A), // Left Option (Alt)
    (0xE3, 0x37), // Left Command (GUI)
    (0xE4, 0x3E), // Right Control
    (0xE5, 0x3C), // Right Shift
    (0xE6, 0x3D), // Right Option
    (0xE7, 0x36), // Right Command
];

/// macOS virtual key code for a HID usage, or `None` for a key macOS has
/// no code for.
///
/// Deliberately unmapped, and why:
///
/// - **Print Screen (0x46), Scroll Lock (0x47), Pause (0x48), Application
///   (0x65), F21-F24 (0x70-0x73)**: no Mac keyboard has them and there is
///   no virtual key code to synthesise. The Windows table maps them
///   because Windows keyboards do have them.
/// - **International2 / 4 / 5 (0x88 カタカナ/ひらがな, 0x8A 変換,
///   0x8B 無変換)**: PC JIS keys with no Mac equivalent. An Apple JIS
///   keyboard has 英数 and かな instead, and reports those as LANG2/LANG1
///   (0x91/0x90), which *are* mapped. Guessing an equivalent here would
///   collide with those two and make the mapping ambiguous in both
///   directions, so it is left out.
pub fn hid_to_virtual_key(hid_usage: u32) -> Option<u16> {
    TABLE
        .iter()
        .find(|(hid, _)| *hid == hid_usage)
        .map(|(_, vk)| *vk)
}

/// HID usage for a macOS virtual key code -- the direction a client on
/// macOS needs to turn an `NSEvent.keyCode` into a spec 2.12 `scancode`.
pub fn virtual_key_to_hid(virtual_key: u16) -> Option<u32> {
    TABLE
        .iter()
        .find(|(_, vk)| *vk == virtual_key)
        .map(|(hid, _)| *hid)
}

/// Whether a HID usage is a modifier key (left/right Control, Shift,
/// Option, Command). macOS delivers these as `flagsChanged` rather than
/// `keyDown`/`keyUp`, and their effect is carried in every later event's
/// flags, so the injector tracks them rather than just forwarding them.
pub fn is_modifier(hid_usage: u32) -> bool {
    (0xE0..=0xE7).contains(&hid_usage)
}

/// Whether a HID usage should carry `NSEventModifierFlagNumericPad`.
/// macOS sets it for the keypad *and* for the arrow keys, which surprises
/// people but is what a real keyboard produces.
pub fn is_numeric_pad(hid_usage: u32) -> bool {
    matches!(hid_usage, 0x4F..=0x52) || matches!(hid_usage, 0x53..=0x63) || hid_usage == 0x67
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_has_no_duplicates_on_either_side() {
        let mut hids = HashSet::new();
        let mut keys = HashSet::new();
        for (hid, vk) in TABLE {
            assert!(hids.insert(*hid), "duplicate HID usage {hid:#x}");
            assert!(keys.insert(*vk), "duplicate virtual key {vk:#x}");
        }
    }

    #[test]
    fn round_trips_every_entry() {
        for (hid, vk) in TABLE {
            assert_eq!(hid_to_virtual_key(*hid), Some(*vk));
            assert_eq!(virtual_key_to_hid(*vk), Some(*hid));
        }
    }

    #[test]
    fn spot_checks() {
        assert_eq!(hid_to_virtual_key(0x04), Some(0x00)); // a -> kVK_ANSI_A
        assert_eq!(hid_to_virtual_key(0x16), Some(0x01)); // s -> kVK_ANSI_S
        assert_eq!(hid_to_virtual_key(0x28), Some(0x24)); // Return
        assert_eq!(hid_to_virtual_key(0x2A), Some(0x33)); // Backspace -> kVK_Delete
        assert_eq!(hid_to_virtual_key(0x4C), Some(0x75)); // Delete -> kVK_ForwardDelete
        assert_eq!(hid_to_virtual_key(0xE3), Some(0x37)); // Left Command
        assert_eq!(hid_to_virtual_key(0x46), None); // Print Screen: no Mac key
        assert_eq!(hid_to_virtual_key(0x8A), None); // JIS 変換: no Mac key
        assert_eq!(virtual_key_to_hid(0x37), Some(0xE3));
    }

    #[test]
    fn backspace_and_delete_do_not_collide() {
        // The classic macOS trap: kVK_Delete (0x33) is Backspace and
        // kVK_ForwardDelete (0x75) is the Delete key.
        assert_ne!(
            hid_to_virtual_key(0x2A).unwrap(),
            hid_to_virtual_key(0x4C).unwrap()
        );
    }

    #[test]
    fn modifiers_and_keypad_are_classified() {
        for hid in 0xE0..=0xE7u32 {
            assert!(is_modifier(hid), "{hid:#x} is a modifier");
        }
        assert!(!is_modifier(0x04));
        assert!(is_numeric_pad(0x52)); // Up arrow
        assert!(is_numeric_pad(0x62)); // Keypad 0
        assert!(is_numeric_pad(0x67)); // Keypad =
        assert!(!is_numeric_pad(0x04)); // a
    }
}
