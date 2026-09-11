//! Permission State Machine (spec 4.5): each `PermissionSet` bit has its
//! own independent state, `NotGranted <-> Granted -> Draining ->
//! NotGranted`, distinguishing `PermissionUpdate.immediate_revoke`
//! (`Granted -> NotGranted` at once) from a staged revoke (`Granted ->
//! Draining`, waiting for in-progress operations on that bit to finish
//! before reaching `NotGranted`).
//!
//! Phase 1 only wires one consumer of this (`VIEW`: revoking it stops the
//! server's video frame send loop, per the PoC brief's minimal-gating
//! ask), but the FSM itself is generic over any `PermissionSet` bit so
//! the immediate/staged distinction it models is a real, tested mechanism
//! rather than a VIEW-specific special case.

use std::collections::HashMap;

use crate::messages::PermissionUpdate;
use crate::permission_set::bit;
use crate::reason_code::ReasonCode;

/// All `PermissionSet` bits this FSM tracks (spec 2.5, DR-033).
const ALL_BITS: [u32; 10] = [
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitState {
    NotGranted,
    Granted,
    /// `Granted -> Draining`: revoked, but not via `immediate_revoke`, so
    /// in-progress operations on this bit may continue (spec 4.5). Only
    /// [`PermissionSm::finish_draining`] can clear this to `NotGranted`;
    /// unlike `NotGranted`, new operations are also refused here.
    Draining,
}

pub struct PermissionSm {
    states: HashMap<u32, BitState>,
}

impl PermissionSm {
    /// `granted_permissions` is the initial state (spec 4.5: "初期状態は
    /// `AuthResult.granted_permissions` に従う").
    pub fn new(granted_permissions: u32) -> Self {
        let states = ALL_BITS
            .into_iter()
            .map(|b| {
                let state = if granted_permissions & b != 0 {
                    BitState::Granted
                } else {
                    BitState::NotGranted
                };
                (b, state)
            })
            .collect();
        Self { states }
    }

    pub fn state(&self, bit: u32) -> BitState {
        *self.states.get(&bit).unwrap_or(&BitState::NotGranted)
    }

    /// `true` only in `Granted` -- both `NotGranted` and `Draining` block
    /// starting a *new* operation gated by `bit` (spec 4.5's table).
    pub fn is_granted(&self, bit: u32) -> bool {
        self.state(bit) == BitState::Granted
    }

    /// Applies a `PermissionUpdate` (spec 2.5): for each bit, being
    /// present in `update.granted_permissions` means `Granted`; being
    /// dropped means `NotGranted` immediately if `update.immediate_revoke`
    /// also contains it, or `Draining` first otherwise.
    pub fn apply_update(&mut self, update: &PermissionUpdate) {
        for b in ALL_BITS {
            let now_granted = update.granted_permissions & b != 0;
            let immediately_revoked = update.immediate_revoke & b != 0;
            let new_state = if now_granted {
                BitState::Granted
            } else if self.state(b) == BitState::Granted {
                if immediately_revoked {
                    BitState::NotGranted
                } else {
                    BitState::Draining
                }
            } else {
                // Already NotGranted/Draining: an update that doesn't
                // grant this bit doesn't change that.
                self.state(b)
            };
            self.states.insert(b, new_state);
        }
    }

    /// Marks in-progress operations gated by `bit` as finished, letting a
    /// `Draining` bit reach `NotGranted` (spec 4.5: "該当する進行中の操作が
    /// すべて完了 -> NotGranted"). A no-op if `bit` isn't `Draining`.
    pub fn finish_draining(&mut self, bit: u32) {
        if self.state(bit) == BitState::Draining {
            self.states.insert(bit, BitState::NotGranted);
        }
    }

    /// Whether starting a *new* operation gated by `bit` is allowed right
    /// now, and if not, the `ReasonCode` spec 4.5's table calls for:
    /// `Draining` (a revoke already in progress, still waiting on
    /// in-progress operations to finish) is `POLICY_PERMISSION_REVOKED`,
    /// while plain `NotGranted` is `POLICY_PERMISSION_DENIED`.
    pub fn check_gate(&self, bit: u32) -> Result<(), ReasonCode> {
        if self.is_granted(bit) {
            return Ok(());
        }
        Err(if self.state(bit) == BitState::Draining {
            ReasonCode::POLICY_PERMISSION_REVOKED
        } else {
            ReasonCode::POLICY_PERMISSION_DENIED
        })
    }
}

/// Builds the `PermissionUpdate` for an admin VIEW grant/revoke toggle
/// (Part 4's minimal live-trigger, wired to `sardp-server`'s
/// `revoke-view`/`grant-view` stdin commands): flips only the VIEW bit,
/// using `immediate_revoke` on a revoke (spec 4.5 -- VIEW has no
/// in-progress operation to drain, so there's no reason to stage it).
/// `current_granted` is the bitmask to preserve every other bit from.
pub fn build_view_toggle(current_granted: u32, grant: bool) -> PermissionUpdate {
    let other_bits = current_granted & !bit::VIEW;
    let view_bit = if grant { bit::VIEW } else { 0 };
    PermissionUpdate {
        granted_permissions: view_bit | other_bits,
        immediate_revoke: if grant { 0 } else { bit::VIEW },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_from_the_initial_grant() {
        let sm = PermissionSm::new(bit::VIEW | bit::INPUT_KEYBOARD);
        assert_eq!(sm.state(bit::VIEW), BitState::Granted);
        assert_eq!(sm.state(bit::INPUT_KEYBOARD), BitState::Granted);
        assert_eq!(sm.state(bit::ADMIN), BitState::NotGranted);
    }

    #[test]
    fn immediate_revoke_goes_straight_to_not_granted() {
        let mut sm = PermissionSm::new(bit::VIEW);
        sm.apply_update(&PermissionUpdate {
            granted_permissions: 0,
            immediate_revoke: bit::VIEW,
        });
        assert_eq!(sm.state(bit::VIEW), BitState::NotGranted);
        assert!(!sm.is_granted(bit::VIEW));
    }

    #[test]
    fn staged_revoke_goes_to_draining_not_not_granted() {
        let mut sm = PermissionSm::new(bit::VIEW);
        sm.apply_update(&PermissionUpdate {
            granted_permissions: 0,
            immediate_revoke: 0, // VIEW dropped, but not immediately
        });
        assert_eq!(
            sm.state(bit::VIEW),
            BitState::Draining,
            "a revoke outside immediate_revoke must land in Draining, not NotGranted"
        );
        // New operations are still refused while Draining, same as
        // NotGranted -- the FSM distinction is about *when the state
        // machine itself considers the bit fully gone*, not about
        // allowing new work in the meantime.
        assert!(!sm.is_granted(bit::VIEW));
    }

    #[test]
    fn draining_only_clears_via_finish_draining() {
        let mut sm = PermissionSm::new(bit::VIEW);
        sm.apply_update(&PermissionUpdate {
            granted_permissions: 0,
            immediate_revoke: 0,
        });
        assert_eq!(sm.state(bit::VIEW), BitState::Draining);
        // A second update that still doesn't grant VIEW must not
        // silently advance it to NotGranted on its own.
        sm.apply_update(&PermissionUpdate {
            granted_permissions: 0,
            immediate_revoke: 0,
        });
        assert_eq!(sm.state(bit::VIEW), BitState::Draining);

        sm.finish_draining(bit::VIEW);
        assert_eq!(sm.state(bit::VIEW), BitState::NotGranted);
    }

    #[test]
    fn finish_draining_is_a_no_op_outside_draining() {
        let mut sm = PermissionSm::new(bit::VIEW);
        sm.finish_draining(bit::VIEW); // still Granted
        assert_eq!(sm.state(bit::VIEW), BitState::Granted);
    }

    #[test]
    fn regrant_after_revoke_returns_to_granted() {
        let mut sm = PermissionSm::new(bit::VIEW);
        sm.apply_update(&PermissionUpdate {
            granted_permissions: 0,
            immediate_revoke: bit::VIEW,
        });
        assert_eq!(sm.state(bit::VIEW), BitState::NotGranted);
        sm.apply_update(&PermissionUpdate {
            granted_permissions: bit::VIEW,
            immediate_revoke: 0,
        });
        assert_eq!(sm.state(bit::VIEW), BitState::Granted);
        assert!(sm.is_granted(bit::VIEW));
    }

    #[test]
    fn unrelated_bits_are_unaffected_by_an_update() {
        let mut sm = PermissionSm::new(bit::VIEW | bit::INPUT_KEYBOARD);
        sm.apply_update(&PermissionUpdate {
            granted_permissions: bit::INPUT_KEYBOARD,
            immediate_revoke: bit::VIEW,
        });
        assert_eq!(sm.state(bit::VIEW), BitState::NotGranted);
        assert_eq!(sm.state(bit::INPUT_KEYBOARD), BitState::Granted);
    }

    #[test]
    fn check_gate_allows_a_granted_bit() {
        let sm = PermissionSm::new(bit::FILE_UP);
        assert_eq!(sm.check_gate(bit::FILE_UP), Ok(()));
    }

    #[test]
    fn check_gate_denies_a_bit_that_was_never_granted() {
        let sm = PermissionSm::new(0);
        assert_eq!(
            sm.check_gate(bit::FILE_DOWN),
            Err(ReasonCode::POLICY_PERMISSION_DENIED)
        );
    }

    #[test]
    fn check_gate_reports_revoked_while_draining() {
        let mut sm = PermissionSm::new(bit::FILE_UP);
        sm.apply_update(&PermissionUpdate {
            granted_permissions: 0,
            immediate_revoke: 0, // staged revoke -> Draining
        });
        assert_eq!(
            sm.check_gate(bit::FILE_UP),
            Err(ReasonCode::POLICY_PERMISSION_REVOKED)
        );
    }

    #[test]
    fn build_view_toggle_grant_sets_view_without_immediate_revoke() {
        let update = build_view_toggle(bit::INPUT_KEYBOARD, true);
        assert_eq!(update.granted_permissions, bit::VIEW | bit::INPUT_KEYBOARD);
        assert_eq!(update.immediate_revoke, 0);
    }

    #[test]
    fn build_view_toggle_revoke_drops_view_and_sets_immediate_revoke() {
        let update = build_view_toggle(bit::VIEW | bit::INPUT_KEYBOARD, false);
        assert_eq!(update.granted_permissions, bit::INPUT_KEYBOARD);
        assert_eq!(update.immediate_revoke, bit::VIEW);
    }
}
