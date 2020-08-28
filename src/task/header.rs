use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::fmt;
use core::task::Waker;

use crate::task::raw::TaskVTable;
use crate::task::state::*;
use crate::task::utils::{abort_on_panic, extend};

/// The header of a task.
///
/// This header is stored right at the beginning of every heap-allocated task.
pub(crate) struct Header {
    /// Current state of the task.
    ///
    /// Contains flags representing the current state and the reference count.
    pub(crate) state: usize,

    /// The task that is blocked on the `JoinHandle`.
    ///
    /// This waker needs to be woken up once the task completes or is closed.
    pub(crate) awaiter: UnsafeCell<Option<Waker>>,

    /// The virtual table.
    ///
    /// In addition to the actual waker virtual table, it also contains pointers to several other
    /// methods necessary for bookkeeping the heap-allocated task.
    pub(crate) vtable: &'static TaskVTable,
}

impl Header {
    /// Cancels the task.
    ///
    /// This method will mark the task as closed, but it won't reschedule the task or drop its
    /// future.
    pub(crate) fn cancel(&mut self) {
        // If the task has been completed or closed, it can't be canceled.
        if self.state & (COMPLETED | CLOSED) != 0 {
            return;
        }

        self.state = CLOSED;
    }

    /// Notifies the awaiter blocked on this task.
    ///
    /// If the awaiter is the same as the current waker, it will not be notified.
    #[inline]
    pub(crate) fn notify(&mut self, current: Option<&Waker>) {
        let state = self.state;
        // Mark the awaiter as being notified.
        self.state |= NOTIFYING;

        // If the awaiter was not being notified nor registered...
        if state & (NOTIFYING | REGISTERING) == 0 {
            // Take the waker out.
            let waker = unsafe { (*self.awaiter.get()).take() };

            // Mark the state as not being notified anymore nor containing an awaiter.
            self.state &= !NOTIFYING & !AWAITER;

            if let Some(w) = waker {
                // We need a safeguard against panics because waking can panic.
                abort_on_panic(|| match current {
                    None => w.wake(),
                    Some(c) if !w.will_wake(c) => w.wake(),
                    Some(_) => {}
                });
            }
        }
    }

    /// Registers a new awaiter blocked on this task.
    ///
    /// This method is called when `JoinHandle` is polled and the task has not completed.
    #[inline]
    pub(crate) fn register(&mut self, waker: &Waker) {
        // Load the state and synchronize with it.
        let state = self.state;

        // There can't be two concurrent registrations because `JoinHandle` can only be polled
        // by a unique pinned reference.
        debug_assert!(state & REGISTERING == 0);

        // If we're in the notifying state at this moment, just wake and return without
        // registering.
        if state & NOTIFYING != 0 {
            abort_on_panic(|| waker.wake_by_ref());
            return;
        }

        self.state |= REGISTERING;

        // Put the waker into the awaiter field.
        unsafe {
            abort_on_panic(|| (*self.awaiter.get()) = Some(waker.clone()));
        }

        // This variable will contain the newly registered waker if a notification comes in before
        // we complete registration.
        let mut waker = None;

        // If there was a notification, take the waker out of the awaiter field.
        if state & NOTIFYING != 0 {
            if let Some(w) = unsafe { (*self.awaiter.get()).take() } {
                abort_on_panic(|| waker = Some(w));
            }
        }

        // The new state is not being notified nor registered, but there might or might not be
        // an awaiter depending on whether there was a concurrent notification.
        let new = if waker.is_none() {
            (state & !NOTIFYING & !REGISTERING) | AWAITER
        } else {
            state & !NOTIFYING & !REGISTERING & !AWAITER
        };

        self.state = new;

        // If there was a notification during registration, wake the awaiter now.
        if let Some(w) = waker {
            abort_on_panic(|| w.wake());
        }
    }

    /// Returns the offset at which the tag of type `T` is stored.
    #[inline]
    pub(crate) fn offset_tag<T>() -> usize {
        let layout_header = Layout::new::<Header>();
        let layout_t = Layout::new::<T>();
        let (_, offset_t) = extend(layout_header, layout_t);
        offset_t
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state;

        f.debug_struct("Header")
            .field("scheduled", &(state & SCHEDULED != 0))
            .field("running", &(state & RUNNING != 0))
            .field("completed", &(state & COMPLETED != 0))
            .field("closed", &(state & CLOSED != 0))
            .field("awaiter", &(state & AWAITER != 0))
            .field("handle", &(state & HANDLE != 0))
            .field("ref_count", &(state / REFERENCE))
            .finish()
    }
}
