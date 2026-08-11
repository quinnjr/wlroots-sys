//! Event delivery, and the reentrancy guard that makes it sound.
//!
//! One `&mut S` reaches handlers. wlroots emits signals synchronously from
//! inside API calls, so a handler that destroys an object re-enters dispatch
//! while that `&mut S` is still live — which aliases `&mut` and is undefined
//! behaviour. The dispatcher detects reentrancy and queues the inner event,
//! draining the queue once the outer handler returns.
//!
//! Two consequences fall out of that and are deliberate:
//!
//! 1. Deferred events carry an [`OutputId`], never a handle, because the object
//!    may be destroyed before delivery. Delivery re-resolves and drops silently
//!    if it is gone.
//! 2. Anything wlroots requires the compositor to complete *before* a callback
//!    returns cannot be deferred. Those paths call handlers directly and say so.

#![cfg_attr(not(test), expect(dead_code))]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use crate::OutputId;

/// An event awaiting delivery.
///
/// Carries ids rather than handles precisely because a deferred event may name
/// an object that no longer exists by the time it is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    NewOutput(OutputId),
    OutputFrame(OutputId),
    OutputDestroyed(OutputId),
}

/// Routes events to handler traits, one at a time.
pub(crate) struct Dispatcher<S> {
    state: *mut S,
    in_dispatch: Cell<bool>,
    deferred: RefCell<VecDeque<Event>>,
}

impl<S> Dispatcher<S> {
    pub(crate) fn new(state: *mut S) -> Self {
        Self {
            state,
            in_dispatch: Cell::new(false),
            deferred: RefCell::new(VecDeque::new()),
        }
    }

    /// True while a handler is running.
    pub(crate) fn is_dispatching(&self) -> bool {
        self.in_dispatch.get()
    }

    /// Deliver `ev`, or queue it if a handler is already running.
    ///
    /// # Safety
    ///
    /// The `*mut S` this dispatcher was built with must still be valid and must
    /// not be aliased by any live reference.
    pub(crate) unsafe fn emit(&self, ev: Event, deliver: fn(&mut S, Event)) {
        if self.in_dispatch.get() {
            self.deferred.borrow_mut().push_back(ev);
            return;
        }

        self.in_dispatch.set(true);

        // SAFETY: the flag above guarantees no other `&mut S` is live for the
        // duration of this call (any reentrant `emit` sees the flag set and
        // queues instead of calling `deliver`), and the caller guarantees the
        // pointer is valid.
        unsafe { deliver(&mut *self.state, ev) };

        // Drain whatever the handler queued. `pop_front` borrows only for the
        // statement, so a handler may queue more while we deliver.
        loop {
            let next = self.deferred.borrow_mut().pop_front();
            match next {
                // SAFETY: as above — still inside the guarded region, so no
                // other `&mut S` can be live.
                Some(ev) => unsafe { deliver(&mut *self.state, ev) },
                None => break,
            }
        }

        self.in_dispatch.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records delivery order, and re-enters the dispatcher from inside a
    /// handler — exactly what wlroots does when a handler destroys an object.
    struct Recorder {
        seen: Vec<Event>,
        reenter_with: RefCell<Option<Event>>,
        dispatcher: *const Dispatcher<Recorder>,
    }

    fn deliver(state: &mut Recorder, ev: Event) {
        state.seen.push(ev);
        if let Some(inner) = state.reenter_with.borrow_mut().take() {
            // SAFETY: the dispatcher outlives the test body.
            unsafe { (*state.dispatcher).emit(inner, deliver) };
        }
    }

    #[test]
    fn reentrant_events_are_deferred_until_the_outer_handler_returns() {
        let mut state = Recorder {
            seen: Vec::new(),
            reenter_with: RefCell::new(Some(Event::OutputDestroyed(OutputId(2)))),
            dispatcher: std::ptr::null(),
        };
        let d = Dispatcher::new(&raw mut state);
        state.dispatcher = &raw const d;

        // SAFETY: `state` outlives `d` for the duration of this call.
        unsafe { d.emit(Event::OutputFrame(OutputId(1)), deliver) };

        assert_eq!(
            state.seen,
            vec![
                Event::OutputFrame(OutputId(1)),
                Event::OutputDestroyed(OutputId(2))
            ],
            "the inner event must arrive after the outer handler returns, not during it"
        );
    }

    #[test]
    fn non_reentrant_events_dispatch_immediately() {
        let mut state = Recorder {
            seen: Vec::new(),
            reenter_with: RefCell::new(None),
            dispatcher: std::ptr::null(),
        };
        let d = Dispatcher::new(&raw mut state);
        state.dispatcher = &raw const d;

        // SAFETY: as above.
        unsafe {
            d.emit(Event::NewOutput(OutputId(7)), deliver);
            d.emit(Event::OutputFrame(OutputId(7)), deliver);
        }

        assert_eq!(
            state.seen,
            vec![
                Event::NewOutput(OutputId(7)),
                Event::OutputFrame(OutputId(7))
            ]
        );
        assert!(
            !d.is_dispatching(),
            "flag must be clear once dispatch unwinds"
        );
    }

    /// A handler-entry/exit marker. Unlike a bare `Event` log, this can tell
    /// deferred delivery apart from naive recursion: both produce the same
    /// final *event* order at one level of reentrancy, but only naive
    /// recursion nests an `Enter` inside another handler's `Enter`/`Exit`
    /// pair.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Trace {
        Enter(Event),
        Exit(Event),
    }

    struct TracingRecorder {
        trace: Vec<Trace>,
        reenter_with: RefCell<Option<Event>>,
        dispatcher: *const Dispatcher<TracingRecorder>,
    }

    fn tracing_deliver(state: &mut TracingRecorder, ev: Event) {
        state.trace.push(Trace::Enter(ev));
        if let Some(inner) = state.reenter_with.borrow_mut().take() {
            // SAFETY: the dispatcher outlives the test body.
            unsafe { (*state.dispatcher).emit(inner, tracing_deliver) };
        }
        state.trace.push(Trace::Exit(ev));
    }

    /// The load-bearing soundness test: proves the inner handler does not
    /// start running until the outer handler has *fully exited*, not merely
    /// that both events eventually appear in the right order. A dispatcher
    /// that recursed into `deliver` directly (instead of deferring) would
    /// produce `Enter(outer), Enter(inner), Exit(inner), Exit(outer)` here —
    /// a nested pair, still with the events in the same final order as the
    /// correct trace, which is exactly what the plain event-order test above
    /// cannot distinguish.
    #[test]
    fn reentrant_handler_fully_exits_before_the_deferred_handler_enters() {
        let mut state = TracingRecorder {
            trace: Vec::new(),
            reenter_with: RefCell::new(Some(Event::OutputDestroyed(OutputId(2)))),
            dispatcher: std::ptr::null(),
        };
        let d = Dispatcher::new(&raw mut state);
        state.dispatcher = &raw const d;

        // SAFETY: `state` outlives `d` for the duration of this call.
        unsafe { d.emit(Event::OutputFrame(OutputId(1)), tracing_deliver) };

        assert_eq!(
            state.trace,
            vec![
                Trace::Enter(Event::OutputFrame(OutputId(1))),
                Trace::Exit(Event::OutputFrame(OutputId(1))),
                Trace::Enter(Event::OutputDestroyed(OutputId(2))),
                Trace::Exit(Event::OutputDestroyed(OutputId(2))),
            ],
            "the inner handler must not start until the outer handler has fully exited"
        );
    }
}
