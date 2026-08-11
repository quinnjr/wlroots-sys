//! Borrow-scoped output handles.
//!
//! An `Output` is valid only for the handler call that produced it. The lifetime
//! `'h` is what enforces that, and the constructor is `pub(crate)` so a consumer
//! cannot manufacture one with a lifetime of their choosing. A handle that
//! escapes a handler is therefore a compile error, not a documented rule.
//!
//! Anything a consumer needs to remember goes in their own state, keyed by
//! [`OutputId`].

use std::ffi::CStr;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::id::{OutputId, find_id};
use crate::{Error, Result, sys};

/// A wlroots output, borrowed for the duration of a handler call.
#[repr(transparent)]
pub struct Output<'h> {
    raw: NonNull<sys::wlr_output>,
    _scope: PhantomData<&'h ()>,
}

impl<'h> Output<'h> {
    /// Wrap a raw output for the duration of a handler call.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `wlr_output` carrying one of our id addons, and the
    /// returned handle must not outlive the callback it was created for.
    ///
    /// Unused outside this module's tests until Task 7 wires up the real
    /// dispatch-time constructors; `expect` (rather than `allow`) makes the
    /// compiler flag this attribute itself as unnecessary the moment those
    /// callers land.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_output) -> Output<'h> {
        Output {
            raw: NonNull::new(raw).expect("wlroots handed us a null output"),
            _scope: PhantomData,
        }
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn as_ptr(&self) -> *mut sys::wlr_output {
        self.raw.as_ptr()
    }

    /// This output's stable identity, safe to store beyond the handler.
    ///
    /// # Panics
    ///
    /// Panics if no id addon is attached. Nothing attaches one yet — that
    /// wiring lands in Task 7, alongside the constructors that call
    /// `Output::from_raw` for real. Until then this method is unreachable
    /// from outside the crate's own tests.
    pub fn id(&self) -> OutputId {
        // SAFETY: the handle's lifetime guarantees the output is live, and an id
        // addon is attached when the output is first seen (Task 7).
        let id = unsafe { find_id(&raw const (*self.raw.as_ptr()).addons) };
        OutputId(id.expect(
            "output has no id addon; it was not registered through the handle constructors \
             wired up in Task 7",
        ))
    }

    /// The output's name, as reported by the backend.
    pub fn name(&self) -> Option<String> {
        // SAFETY: the handle's lifetime guarantees the output is live. wlroots
        // may leave `name` null before the output is configured.
        unsafe {
            let name = (*self.raw.as_ptr()).name;
            if name.is_null() {
                return None;
            }
            Some(CStr::from_ptr(name).to_string_lossy().into_owned())
        }
    }

    /// Commit the output's pending state.
    ///
    /// wlroots replaced the implicit pending-state model with an explicit
    /// `wlr_output_state` part-way through the versions this project supports:
    /// 0.15 has only `wlr_output_commit`, 0.19 and later only
    /// `wlr_output_commit_state`, and 0.17 carries both during the transition.
    /// This branch binds 0.20, so it uses the newer call; the `support/*`
    /// branches differ *inside this method* and keep the signature identical,
    /// which is what lets a consumer move between them by changing a version.
    pub fn commit(&self) -> Result<()> {
        // SAFETY: the handle's lifetime guarantees the output is live. The
        // state is initialised before use and finished before it drops, as
        // wlroots requires.
        unsafe {
            let mut state = std::mem::zeroed::<sys::wlr_output_state>();
            sys::wlr_output_state_init(&raw mut state);
            let ok = sys::wlr_output_commit_state(self.raw.as_ptr(), &raw const state);
            sys::wlr_output_state_finish(&raw mut state);

            if ok {
                Ok(())
            } else {
                Err(Error::Operation("wlr_output_commit_state"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{Layout, alloc_zeroed, dealloc};

    /// A zeroed, heap-allocated `wlr_output`, freed on drop.
    ///
    /// `wlr_output` embeds `wl_listener`s, which carry bare (non-`Option`)
    /// function pointers — a type `std::mem::zeroed` refuses to produce, since
    /// a zero function pointer is itself already UB to materialise as a
    /// *value*. Allocating the bytes directly and only ever touching them
    /// through a raw pointer (never loading a whole `wlr_output` into a Rust
    /// place) sidesteps that: nothing here reads a listener's `notify` field,
    /// so the invalid bit pattern is never observed as the type it names.
    struct ScratchOutput(*mut sys::wlr_output);

    impl ScratchOutput {
        fn new() -> Self {
            let layout = Layout::new::<sys::wlr_output>();
            // SAFETY: `layout` is non-zero-sized (`wlr_output` has fields), so
            // `alloc_zeroed` returns either null (checked below) or a
            // suitably aligned, zeroed allocation of exactly that size.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_output>();
            assert!(!ptr.is_null(), "allocation failed");
            // SAFETY: `ptr` is a fresh, exclusively-owned, zeroed allocation
            // sized for `wlr_addon_set`'s enclosing type; `wlr_addon_set_init`
            // only writes the two `wl_list` fields it owns, which is in
            // bounds of that allocation.
            unsafe { sys::wlr_addon_set_init(&raw mut (*ptr).addons) };
            Self(ptr)
        }
    }

    impl Drop for ScratchOutput {
        fn drop(&mut self) {
            // SAFETY: `self.0`'s addon set was initialised in `new` and has
            // no addons attached by these tests beyond what each test itself
            // manages, so finishing it here is exactly undoing that `init`.
            unsafe { sys::wlr_addon_set_finish(&raw mut (*self.0).addons) };
            // SAFETY: `self.0` was allocated by `alloc_zeroed` with this same
            // layout in `new`, and is not used again after this point.
            unsafe { dealloc(self.0.cast::<u8>(), Layout::new::<sys::wlr_output>()) };
        }
    }

    /// Exercises `from_raw`/`as_ptr` against a standalone `wlr_output`, the
    /// same style `id::tests` uses for a standalone `wlr_addon_set`: no
    /// display, backend, or real output is needed to prove the pointer plumbing.
    #[test]
    fn from_raw_wraps_and_as_ptr_recovers_the_same_pointer() {
        let output = ScratchOutput::new();

        // SAFETY: `output.0` is a live `wlr_output` with an initialised addon
        // set, and the handle does not outlive this function.
        let handle = unsafe { Output::from_raw(output.0) };
        assert_eq!(
            handle.as_ptr(),
            output.0,
            "from_raw must not copy or offset"
        );
        assert_eq!(handle.name(), None, "zeroed output has a null name");
    }

    /// The panic message this produces is read by a Task 7 author the moment
    /// they wire up a constructor that forgot to attach an id addon first —
    /// it must name what is missing, not just that `id()` failed.
    #[test]
    #[should_panic(expected = "output has no id addon")]
    fn id_panics_when_no_addon_is_attached() {
        let output = ScratchOutput::new();
        // SAFETY: as in the test above.
        let handle = unsafe { Output::from_raw(output.0) };
        let _ = handle.id();
    }
}
