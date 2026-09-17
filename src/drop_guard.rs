//! A generic RAII guard: runs a closure exactly once, on drop -- whichever
//! way the scope ends (normal return, an early `?`, or an unwinding
//! panic).
//!
//! Every OS-integration crate (`sardp-win`, `sardp-mac`, and the
//! `tools/*-capture-poc` validation binaries) has grown its own
//! single-purpose `impl Drop` for exactly this shape: "call this one
//! cleanup function when I go out of scope" (KNOWN_ISSUES #29's 2026-09-14
//! survey counted 13 of them). [`DropGuard`] is the common replacement for
//! the ones that are nothing but that -- a closure, run once. It does not
//! replace every `impl Drop` in those crates: a type whose `Drop` shares
//! state with its own methods (a `finished`/`finalized` flag guarding a
//! `finish()` call against being invoked twice, e.g. the MFT `Encoder`
//! types) or that releases a raw FFI handle through a type-erased function
//! pointer captured at construction (the macOS PoC's `sck-capture-poc`
//! sinks) doesn't fit a closure-based guard any better than its current
//! hand-written form, and is left as is.

/// Runs `action` exactly once, when the guard is dropped.
///
/// ```
/// # use sardp::drop_guard::DropGuard;
/// # use std::cell::Cell;
/// let released = Cell::new(false);
/// {
///     let _guard = DropGuard::new(|| released.set(true));
///     assert!(!released.get());
/// }
/// assert!(released.get());
/// ```
#[must_use = "a DropGuard's action runs when it is dropped; bind it to a \
              named variable (`let _guard = DropGuard::new(...)`) to keep \
              it alive for the rest of the scope -- `let _ = DropGuard::new(...)` \
              or a bare statement drops it immediately, running the action \
              right away instead of at scope exit"]
pub struct DropGuard<F: FnOnce()> {
    // `Option` (rather than `MaybeUninit`/`ManuallyDrop`) so `Drop::drop`
    // can `take()` it: `F` isn't `Copy`, and `drop(&mut self)` only ever
    // gets `&mut self`, not ownership of `self`, so taking the closure out
    // is the only way to call it by value.
    action: Option<F>,
}

impl<F: FnOnce()> DropGuard<F> {
    /// Runs `action` when the returned guard is dropped.
    pub fn new(action: F) -> Self {
        Self {
            action: Some(action),
        }
    }
}

impl<F: FnOnce()> Drop for DropGuard<F> {
    fn drop(&mut self) {
        if let Some(action) = self.action.take() {
            action();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::panic::{self, AssertUnwindSafe};

    #[test]
    fn runs_the_action_on_scope_exit() {
        let ran = Cell::new(false);
        {
            let _guard = DropGuard::new(|| ran.set(true));
            assert!(!ran.get(), "must not run before the guard drops");
        }
        assert!(ran.get(), "must run once the guard drops");
    }

    #[test]
    fn runs_on_early_return_via_the_try_operator() {
        fn inner(ran: &Cell<bool>) -> Result<(), ()> {
            let _guard = DropGuard::new(|| ran.set(true));
            Err(())?;
            unreachable!();
        }
        let ran = Cell::new(false);
        let _ = inner(&ran);
        assert!(
            ran.get(),
            "must run on an early `?` return, same as any other Drop"
        );
    }

    #[test]
    fn runs_exactly_once_even_if_the_scope_unwinds() {
        let count = Cell::new(0u32);
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = DropGuard::new(|| count.set(count.get() + 1));
            panic!("simulated failure mid-scope");
        }));
        assert!(
            result.is_err(),
            "the panic should propagate to catch_unwind"
        );
        assert_eq!(
            count.get(),
            1,
            "the action must run exactly once, not be skipped by the unwind"
        );
    }

    #[test]
    fn an_immediately_dropped_guard_runs_right_away() {
        // Documented in the #[must_use] message: a guard not bound to a
        // named variable drops (and so runs) at the end of its statement,
        // not at the end of the enclosing scope. Exercised here so that
        // behavior stays intentional, not accidental.
        let ran = Cell::new(false);
        let _ = DropGuard::new(|| ran.set(true));
        assert!(ran.get());
    }

    #[test]
    fn a_move_only_capture_works() {
        // The closure owns what it captures (FnOnce, not Fn/FnMut), so a
        // guard can release something it was handed ownership of -- the
        // exact shape every replaced call site (NetemGuard, FrameGuard,
        // Presenter, UserDataGuard) needs.
        struct DropSpy<'a>(&'a Cell<u32>);
        impl Drop for DropSpy<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let drops = Cell::new(0);
        let owned = DropSpy(&drops);
        {
            let _guard = DropGuard::new(move || drop(owned));
            assert_eq!(drops.get(), 0);
        }
        assert_eq!(drops.get(), 1);
    }
}
