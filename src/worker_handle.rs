//! A worker OS thread fed through a channel: [`WorkerHandle`] owns the
//! sending half and the thread's `JoinHandle`, so stopping it on drop is
//! "close the channel from this side, then join" -- the shape
//! `sardp-win`'s `InputInjector` and `H264DisplayWindow` each had as a
//! hand-written `impl Drop` before KNOWN_ISSUES #29's consolidation.
//!
//! This is a narrower relative of [`crate::frame_source::FrameWorker`]:
//! that one owns the *receiving* half of an async channel the worker
//! thread produces into (plus a readiness handshake for the info the
//! worker discovers once it starts). This one is the other direction --
//! the *sending* half of a plain channel the caller feeds the worker
//! through, with no readiness protocol of its own -- which is what
//! `InputInjector`/`H264DisplayWindow` actually needed.

use std::sync::mpsc::{SendError, Sender};
use std::thread::JoinHandle;

/// Owns a worker thread's inbound channel and its `JoinHandle`. Dropping
/// it closes the channel from this side and then joins the thread, so
/// the drop blocks until the worker has actually exited -- whichever way
/// the scope holding the handle ends.
///
/// Closing the channel is what lets a worker blocked in a blocking
/// receive (`for msg in receiver`, `InputInjector`'s shape) notice and
/// return. A worker that instead watches some other shutdown signal
/// (`H264DisplayWindow`'s `AtomicBool`, set by its own `Drop` -- which,
/// since a manual `Drop::drop` always runs before a struct's field-wise
/// auto-drop, has already happened by the time a `WorkerHandle` field's
/// own drop runs) just sees the channel close as an unsurprising side
/// effect and exits its loop the same way either kind of worker does.
pub struct WorkerHandle<T> {
    tx: Option<Sender<T>>,
    worker: Option<JoinHandle<()>>,
}

impl<T> WorkerHandle<T> {
    /// `tx` is the channel end the worker thread reads from; `worker` is
    /// its `JoinHandle`.
    pub fn new(tx: Sender<T>, worker: JoinHandle<()>) -> Self {
        Self {
            tx: Some(tx),
            worker: Some(worker),
        }
    }

    /// Queues `item` for the worker thread. Never blocks. `Err` only once
    /// the worker's receiver is gone -- it exited on its own (returned,
    /// or panicked) -- not merely because a drop of `self` happens to be
    /// pending: the only place `tx` is ever taken is `Drop::drop`, which
    /// requires exclusive ownership of `self` and so can't overlap with a
    /// `&self` call to this method.
    pub fn send(&self, item: T) -> Result<(), SendError<T>> {
        self.tx
            .as_ref()
            .expect("tx is only ever taken by Drop, which consumes self")
            .send(item)
    }
}

impl<T> Drop for WorkerHandle<T> {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Spawns a thread that sums everything sent to it (a stand-in for
    /// `InputInjector`'s "consume commands until the channel closes"
    /// worker) and reports the total through `total` once its loop ends.
    fn spawn_summing_worker(total: Arc<AtomicU32>) -> WorkerHandle<u32> {
        let (tx, rx) = mpsc::channel::<u32>();
        let worker = std::thread::spawn(move || {
            let mut sum = 0u32;
            for item in rx {
                sum += item;
            }
            total.store(sum, Ordering::SeqCst);
        });
        WorkerHandle::new(tx, worker)
    }

    #[test]
    fn sent_items_reach_the_worker_before_drop_joins() {
        let total = Arc::new(AtomicU32::new(0));
        let handle = spawn_summing_worker(total.clone());
        handle.send(1).expect("worker is alive");
        handle.send(2).expect("worker is alive");
        handle.send(3).expect("worker is alive");
        drop(handle); // closes the channel, then blocks until the thread exits
        assert_eq!(
            total.load(Ordering::SeqCst),
            6,
            "drop must have waited for the worker to finish"
        );
    }

    #[test]
    fn drop_without_ever_sending_still_joins_cleanly() {
        let total = Arc::new(AtomicU32::new(0));
        let handle = spawn_summing_worker(total.clone());
        drop(handle);
        assert_eq!(total.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn send_fails_once_the_worker_has_exited_on_its_own() {
        let (tx, rx) = mpsc::channel::<u32>();
        // A worker that returns immediately, dropping its receiver -- the
        // "exited on its own" case `WorkerHandle` itself can't prevent.
        let worker = std::thread::spawn(move || drop(rx));
        let handle = WorkerHandle::new(tx, worker);
        // Give the thread a moment to actually run and drop its receiver;
        // a `send` racing that is still well-defined (either observes the
        // closed channel or briefly succeeds into a buffer nobody reads),
        // but the deterministic assertion below needs the thread to have
        // finished first.
        std::thread::sleep(Duration::from_millis(50));
        assert!(handle.send(1).is_err(), "the worker's receiver is gone");
    }
}
