//! Input-side session state (spec 4.4), OS-independent:
//!
//! - [`ImeModeSm`]: the per-session IME mode state machine (spec 4.4.1),
//!   including the `effective_after_event_id` boundary rule.
//! - [`PressedInputs`]: the pressed key/button invariant (spec 4.4.2) --
//!   whoever injects input must be able to synthesize the matching
//!   releases when the `input` stream or the connection goes away, so no
//!   key stays stuck down on the remote desktop.
//! - [`is_character_key`]: which physical keys the server must *not*
//!   inject as key events while the client composes text, because spec
//!   2.12 requires characters to come from `TextInput` only (injecting the
//!   scancode of a printable key would make the remote OS generate the
//!   character a second time).

use std::collections::BTreeSet;

use crate::messages::{ImeMode, ImeModeChange, key_modifier};
use crate::reason_code::ReasonCode;

/// Spec 4.4.1: one per session, default `CLIENT_SIDE`. A mode change
/// takes effect for events with `event_id > effective_after_event_id`;
/// events at or below it are still processed under the old mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImeModeSm {
    current: ImeMode,
    /// A change announced but not yet crossed (`(new mode, N)`). Only the
    /// most recent announcement is kept: the client can't usefully queue
    /// two flips on a stream that delivers events in order anyway.
    pending: Option<(ImeMode, u64)>,
}

impl Default for ImeModeSm {
    fn default() -> Self {
        Self::new()
    }
}

impl ImeModeSm {
    pub fn new() -> Self {
        Self {
            current: ImeMode::ClientSide,
            pending: None,
        }
    }

    pub fn on_mode_change(&mut self, change: &ImeModeChange) {
        self.pending = Some((change.mode, change.effective_after_event_id));
    }

    /// The mode an event with `event_id` is processed under; crosses a
    /// pending boundary when `event_id > N`.
    pub fn mode_for(&mut self, event_id: u64) -> ImeMode {
        if let Some((mode, boundary)) = self.pending
            && event_id > boundary
        {
            self.current = mode;
            self.pending = None;
        }
        self.current
    }

    /// The mode for events not yet seen (what a fresh event would get if
    /// no boundary is pending).
    pub fn current(&self) -> ImeMode {
        self.current
    }

    /// Spec 4.4.1's forbidden-message rule: `TextInput`/`ImeComposition`
    /// under `REMOTE_SIDE` is `PROTOCOL.UNEXPECTED_MESSAGE`.
    pub fn check_text_allowed(mode: ImeMode) -> Result<(), ReasonCode> {
        match mode {
            ImeMode::ClientSide => Ok(()),
            ImeMode::RemoteSide => Err(ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE),
        }
    }
}

/// Something the injecting side must release to restore the spec 4.4.2
/// invariant (all keys/buttons `Idle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// A key, by USB HID usage (`KeyEvent.scancode`).
    Key(u32),
    /// A mouse button (`MouseButton.button`).
    Button(u8),
}

/// Spec 4.4.2: the set of keys/buttons currently down, tracked from the
/// `down` flags of the events that were actually injected (or, on the
/// client, actually sent).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PressedInputs {
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u8>,
}

impl PressedInputs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_key(&mut self, scancode: u32, down: bool) {
        if down {
            self.keys.insert(scancode);
        } else {
            self.keys.remove(&scancode);
        }
    }

    pub fn on_button(&mut self, button: u8, down: bool) {
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// The releases needed to bring everything back to `Idle`, and marks
    /// them as released. Buttons first, then keys (so a drag ends before
    /// its modifier goes up).
    pub fn take_releases(&mut self) -> Vec<Release> {
        let mut releases: Vec<Release> = self.buttons.iter().map(|b| Release::Button(*b)).collect();
        releases.extend(self.keys.iter().map(|k| Release::Key(*k)));
        self.buttons.clear();
        self.keys.clear();
        releases
    }
}

/// Whether a physical key (USB HID usage, keyboard/keypad page 0x07)
/// normally produces a character when pressed on its own: letters,
/// digits, punctuation, space, keypad digits/operators, and the
/// language-specific character keys (JIS `\_`/`¥`, ISO `<>`). Not:
/// Enter/Tab/Backspace/Escape (their control characters are filtered
/// from `TextInput` on the client and travel as `KeyEvent`s), modifiers,
/// navigation, function and lock keys, keypad Enter.
pub fn is_character_key(hid_usage: u32) -> bool {
    matches!(
        hid_usage,
        0x04..=0x27          // a-z, 1-9, 0
        | 0x2C               // space
        | 0x2D..=0x38        // - = [ ] \ # ; ' ` , . /
        | 0x54..=0x57        // keypad / * - +
        | 0x59..=0x63        // keypad 1-9 0 .
        | 0x64               // non-US \ |
        | 0x67               // keypad =
        | 0x85               // keypad ,
        | 0x87               // international1 (JIS \ _ ろ)
        | 0x89               // international3 (JIS ¥ |)
    )
}

/// Server-side decision for a `KeyEvent` (spec 2.12 + 4.4.1): under
/// `CLIENT_SIDE`, a character key pressed without Ctrl/Alt/Meta is not
/// injected -- its character arrives as `TextInput`. Everything else
/// (shortcuts, modifiers, navigation, and all keys under `REMOTE_SIDE`)
/// is injected as a physical key.
pub fn should_inject_key(mode: ImeMode, hid_usage: u32, modifiers: u16) -> bool {
    match mode {
        ImeMode::RemoteSide => true,
        ImeMode::ClientSide => {
            let shortcut_modifier =
                modifiers & (key_modifier::CTRL | key_modifier::ALT | key_modifier::META) != 0;
            shortcut_modifier || !is_character_key(hid_usage)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_is_client_side() {
        let mut sm = ImeModeSm::new();
        assert_eq!(sm.current(), ImeMode::ClientSide);
        assert_eq!(sm.mode_for(1), ImeMode::ClientSide);
    }

    #[test]
    fn mode_change_applies_strictly_after_the_boundary_event_id() {
        let mut sm = ImeModeSm::new();
        sm.on_mode_change(&ImeModeChange {
            mode: ImeMode::RemoteSide,
            effective_after_event_id: 10,
        });
        // Still the old mode up to and including N (spec 4.4.1 MUST).
        assert_eq!(sm.mode_for(9), ImeMode::ClientSide);
        assert_eq!(sm.mode_for(10), ImeMode::ClientSide);
        assert_eq!(sm.current(), ImeMode::ClientSide);
        // New mode from N+1 on.
        assert_eq!(sm.mode_for(11), ImeMode::RemoteSide);
        assert_eq!(sm.current(), ImeMode::RemoteSide);
        assert_eq!(sm.mode_for(12), ImeMode::RemoteSide);
    }

    #[test]
    fn a_later_announcement_replaces_a_pending_one() {
        let mut sm = ImeModeSm::new();
        sm.on_mode_change(&ImeModeChange {
            mode: ImeMode::RemoteSide,
            effective_after_event_id: 10,
        });
        sm.on_mode_change(&ImeModeChange {
            mode: ImeMode::ClientSide,
            effective_after_event_id: 20,
        });
        assert_eq!(sm.mode_for(15), ImeMode::ClientSide);
        assert_eq!(sm.mode_for(21), ImeMode::ClientSide);
    }

    #[test]
    fn text_is_a_violation_under_remote_side() {
        assert_eq!(ImeModeSm::check_text_allowed(ImeMode::ClientSide), Ok(()));
        assert_eq!(
            ImeModeSm::check_text_allowed(ImeMode::RemoteSide),
            Err(ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE)
        );
    }

    #[test]
    fn releases_cover_everything_still_down_buttons_first() {
        let mut pressed = PressedInputs::new();
        pressed.on_key(0xE0, true); // LCtrl
        pressed.on_key(0x06, true); // c
        pressed.on_key(0x06, false);
        pressed.on_button(1, true);
        assert!(!pressed.is_empty());
        assert_eq!(
            pressed.take_releases(),
            vec![Release::Button(1), Release::Key(0xE0)]
        );
        assert!(pressed.is_empty());
        assert!(pressed.take_releases().is_empty());
    }

    #[test]
    fn character_keys_are_the_printable_ones() {
        assert!(is_character_key(0x04)); // a
        assert!(is_character_key(0x27)); // 0
        assert!(is_character_key(0x2C)); // space
        assert!(is_character_key(0x38)); // /
        assert!(is_character_key(0x59)); // keypad 1
        assert!(is_character_key(0x89)); // JIS yen
        assert!(!is_character_key(0x28)); // Enter
        assert!(!is_character_key(0x2A)); // Backspace
        assert!(!is_character_key(0x2B)); // Tab
        assert!(!is_character_key(0x29)); // Escape
        assert!(!is_character_key(0x3A)); // F1
        assert!(!is_character_key(0x4F)); // Right arrow
        assert!(!is_character_key(0x58)); // keypad Enter
        assert!(!is_character_key(0xE1)); // LShift
    }

    #[test]
    fn character_keys_are_injected_only_as_shortcuts_under_client_side() {
        // Plain 'a': the character comes via TextInput, don't inject.
        assert!(!should_inject_key(ImeMode::ClientSide, 0x04, 0));
        // Shift+'a' is still a character ('A' via TextInput).
        assert!(!should_inject_key(
            ImeMode::ClientSide,
            0x04,
            key_modifier::SHIFT
        ));
        // Ctrl+'c' is a shortcut: inject the physical key.
        assert!(should_inject_key(
            ImeMode::ClientSide,
            0x06,
            key_modifier::CTRL
        ));
        assert!(should_inject_key(
            ImeMode::ClientSide,
            0x04,
            key_modifier::ALT
        ));
        assert!(should_inject_key(
            ImeMode::ClientSide,
            0x04,
            key_modifier::META
        ));
        // Non-character keys always go through.
        assert!(should_inject_key(ImeMode::ClientSide, 0x28, 0));
        assert!(should_inject_key(ImeMode::ClientSide, 0xE1, 0));
        // REMOTE_SIDE: everything is a physical key.
        assert!(should_inject_key(ImeMode::RemoteSide, 0x04, 0));
    }
}
