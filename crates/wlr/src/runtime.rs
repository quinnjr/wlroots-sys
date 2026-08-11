//! The long-lived side of a compositor: everything a run wires up, and every
//! operation that names an object by id.
//!
//! # Why this type exists
//!
//! Handles cannot be stored (see the crate docs), handlers receive only
//! `&mut S`, and a `Dispatcher<S>` exists only for the duration of one
//! [`Backend::run_all`](crate::Backend::run_all) call. Three consequences
//! follow, and this type is the answer to all three:
//!
//! 1. An fd source cannot be registered against the event loop before a run,
//!    because the C callback has no dispatcher to reach. So sources are
//!    *declared* here and **registered by each run**, torn down when it
//!    returns, and re-armed by the next one — the same lifetime the per-output
//!    listeners already have.
//! 2. A mutation that names an object by id (`set_toplevel_size`, and its
//!    siblings from 0.20.2 on) has to be callable from a handler, which can
//!    reach nothing but its own `&mut S`. So `Runtime` is `Clone` and cheap:
//!    a consumer keeps a clone in their state and calls through it.
//! 3. The tables that turn an id back into a live object outlive any one
//!    handler call but must be readable during one, so they are `RefCell`s.
//!    **No borrow may be held across a call into consumer code.** Copy the
//!    pointer out, drop the borrow, then call — `backend.rs`'s `with_output`
//!    is the pattern. A double borrow inside an `extern "C"` frame is an
//!    abort, not a caught panic.
//!
//! `Runtime` is `!Send`/`!Sync` (its `Rc` and `NonNull` fields see to that),
//! which the thread-scoped dispatch guard in `dispatch.rs` depends on.

use std::cell::RefCell;
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

use crate::id::{SourceId, next_id};
use crate::{Interest, Result};

/// A declared fd source: the descriptor, what it wants, and its id.
///
/// Owns the `OwnedFd` so the descriptor cannot be closed while a run has it
/// registered with the event loop — the one hazard a `RawFd` parameter would
/// have left open, and one no amount of documentation prevents.
pub(crate) struct FdSource {
    pub(crate) fd: OwnedFd,
    pub(crate) interest: Interest,
    pub(crate) id: SourceId,
}

pub(crate) struct RuntimeInner {
    pub(crate) sources: RefCell<Vec<FdSource>>,
}

/// Handle to a compositor's long-lived wlroots state.
///
/// Cheap to clone (one `Rc` bump). Every clone names the same underlying
/// state, so a clone kept in a consumer's own state and the one passed to
/// [`Backend::run_all`](crate::Backend::run_all) are interchangeable.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) inner: Rc<RuntimeInner>,
}

impl Runtime {
    /// Create an empty runtime.
    ///
    /// # Errors
    ///
    /// None today. It returns [`Result`] because 0.20.2 gives this call real
    /// work to do (creating the scene graph and the output layout, both of
    /// which can fail), and widening an infallible signature to a fallible one
    /// later would be a breaking change this crate has no version in which to
    /// make.
    pub fn new() -> Result<Runtime> {
        Ok(Runtime {
            inner: Rc::new(RuntimeInner {
                sources: RefCell::new(Vec::new()),
            }),
        })
    }

    /// Declare `fd` as an event source, watched for `interest`.
    ///
    /// The runtime takes ownership of the descriptor and closes it when the
    /// last clone of this handle drops. Handlers get it back as a
    /// [`BorrowedFd`](std::os::fd::BorrowedFd) in
    /// [`FdHandler::fd_ready`](crate::FdHandler::fd_ready).
    ///
    /// Registration with the event loop happens inside
    /// [`Backend::run_all`](crate::Backend::run_all) and lives for exactly
    /// that call, so declaring a source during a run has no effect until the
    /// next one. There is no removal by id in 0.20.1; a source lives as long
    /// as the runtime.
    pub fn add_fd(&self, fd: OwnedFd, interest: Interest) -> SourceId {
        let id = SourceId(next_id());
        self.inner.sources.borrow_mut().push(FdSource { fd, interest, id });
        id
    }

    /// The descriptor `id` names, borrowed for the callback that resolves it.
    ///
    /// Returns `None` for an id this runtime never issued, which delivery
    /// treats as "drop the event" rather than as a fault — the same rule
    /// output delivery follows for a destroyed output.
    pub(crate) fn with_fd<R>(
        &self,
        id: SourceId,
        f: impl FnOnce(std::os::fd::BorrowedFd<'_>) -> R,
    ) -> Option<R> {
        // The borrow ends before `f` runs: `f` is consumer code, which can
        // call back into this runtime and take the same `RefCell` mutably.
        let raw = {
            let sources = self.inner.sources.borrow();
            sources.iter().find(|s| s.id == id).map(|s| s.fd.as_raw_fd())
        }?;
        // SAFETY: `raw` came from an `OwnedFd` this runtime owns, and this
        // handle keeps that `OwnedFd` alive for the whole call — nothing
        // removes a source in 0.20.1, and `f` cannot reach one if it did.
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
        Some(f(borrowed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_not_impl_any;

    // The premise `dispatch.rs`'s thread-local guard rests on. An `Rc` and a
    // `RefCell` give this incidentally today; a future `Arc` field, or a
    // well-meant `unsafe impl Send`, would void the guard in silence.
    assert_not_impl_any!(Runtime: Send, Sync);

    fn pipe_read_end() -> OwnedFd {
        let (read, _write) = rustix::pipe::pipe().expect("pipe");
        read
    }

    #[test]
    fn ids_are_unique_and_resolve_to_the_fd_they_were_issued_for() {
        let rt = Runtime::new().expect("runtime");
        let a = rt.add_fd(pipe_read_end(), Interest::READABLE);
        let b = rt.add_fd(pipe_read_end(), Interest::READABLE);
        assert_ne!(a, b);

        let a_raw = rt.with_fd(a, |fd| fd.as_raw_fd()).expect("a resolves");
        let b_raw = rt.with_fd(b, |fd| fd.as_raw_fd()).expect("b resolves");
        assert_ne!(a_raw, b_raw, "each id names its own descriptor");
    }

    #[test]
    fn an_unknown_id_resolves_to_nothing_rather_than_panicking() {
        let rt = Runtime::new().expect("runtime");
        assert!(rt.with_fd(SourceId(u64::MAX), |_| ()).is_none());
    }

    /// Clones must share state, or a consumer's stored clone and the one
    /// `run_all` was given would disagree about which sources exist.
    #[test]
    fn a_clone_sees_sources_added_through_the_original() {
        let rt = Runtime::new().expect("runtime");
        let clone = rt.clone();
        let id = rt.add_fd(pipe_read_end(), Interest::READABLE);
        assert!(clone.with_fd(id, |_| ()).is_some());
    }
}
