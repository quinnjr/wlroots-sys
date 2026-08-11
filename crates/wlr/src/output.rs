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
///
/// Not `#[repr(transparent)]`: this holds the output's id alongside the raw
/// pointer, cached when the handle is built, so [`id`](Output::id) is a field
/// read rather than an FFI round trip through `wlr_addon_find` on every call.
/// Dispatch already knows the id — it used it to look this output up in the
/// registry — so it hands that value straight to the handle instead of making
/// `id()` re-derive it, which matters because keying your own state by id is
/// the sanctioned pattern and so `id()` runs on essentially every event. The
/// field is private, so carrying it is not a change to the public API.
pub struct Output<'h> {
    raw: NonNull<sys::wlr_output>,
    id: Option<OutputId>,
    _scope: PhantomData<&'h ()>,
}

impl<'h> Output<'h> {
    /// Wrap a raw output for the duration of a handler call.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `wlr_output` with an initialised addon set, and
    /// the returned handle must not outlive the callback it was created for.
    /// An id addon is not required here: [`id`](Output::id) tolerates its
    /// absence (it panics rather than reading invalid memory), so attaching
    /// one is a correctness concern for callers of `id`, not a soundness
    /// precondition of this constructor.
    ///
    /// The id is not cached by this constructor — it is looked up (and can
    /// panic) the first time [`id`](Output::id) is called. Callers that
    /// already know the id should use [`from_raw_with_id`](Output::from_raw_with_id)
    /// instead, which avoids both the lookup and the possibility of that
    /// panic.
    ///
    /// Dispatch no longer calls this directly (it uses `from_raw_with_id`,
    /// which it already has the id for); it stays `pub(crate)` and exercised
    /// because `output.rs`'s own tests need a constructor that does *not*
    /// pre-attach an id, to cover the panicking path in [`id`](Output::id).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_output) -> Output<'h> {
        Output {
            raw: NonNull::new(raw).expect("wlroots handed us a null output"),
            id: None,
            _scope: PhantomData,
        }
    }

    /// Wrap a raw output for the duration of a handler call, attaching an
    /// already-known id.
    ///
    /// This is what dispatch uses: it resolves the [`OutputId`] to look the
    /// output up in the session registry, and can hand that same value
    /// straight to the handle instead of making [`id`](Output::id) re-derive
    /// it later through `wlr_addon_find`.
    ///
    /// # Safety
    ///
    /// Same as [`from_raw`](Output::from_raw): `raw` must be a live
    /// `wlr_output`, and the returned handle must not outlive the callback it
    /// was created for.
    pub(crate) unsafe fn from_raw_with_id(raw: *mut sys::wlr_output, id: OutputId) -> Output<'h> {
        Output {
            raw: NonNull::new(raw).expect("wlroots handed us a null output"),
            id: Some(id),
            _scope: PhantomData,
        }
    }

    /// This output's stable identity, safe to store beyond the handler.
    ///
    /// # Panics
    ///
    /// Panics if the output carries no id addon. Unreachable for a handle a
    /// handler was given: dispatch caches the id into the handle when it
    /// builds it, and every output this crate hands out had an id attached
    /// when wlroots announced it, before any handler could see it. The panic
    /// exists for the crate's own tests, which construct handles by a path
    /// dispatch never uses.
    pub fn id(&self) -> OutputId {
        if let Some(id) = self.id {
            return id;
        }
        // SAFETY: the handle's lifetime guarantees the output is live.
        let id = unsafe { find_id(&raw const (*self.raw.as_ptr()).addons) };
        OutputId(id.expect(
            "output has no id addon; it was not registered by the dispatch-time constructor \
             that attaches its id addon",
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

    /// Commit an empty state to the output.
    ///
    /// There is no *pending* state to commit, and that is not an omission here
    /// but a change in wlroots: it replaced the implicit pending-state model
    /// with an explicit `wlr_output_state` part-way through the versions this
    /// project supports. 0.15 has only `wlr_output_commit`, 0.19 and later only
    /// `wlr_output_commit_state`, and 0.17 carries both during the transition.
    /// This branch binds 0.20, so it initialises a fresh `wlr_output_state`,
    /// commits that, and finishes it. The `support/*` branches differ *inside
    /// this method* and keep the signature identical, which is what lets a
    /// consumer move between them by changing a version.
    ///
    /// Committing an empty state applies no changes, so on a disabled output
    /// this is observably a no-op. That is the whole of what this slice offers:
    /// the `wlr_output_state` setters — enabling an output, setting a mode,
    /// attaching a buffer — are not exposed yet and arrive with the rendering
    /// slice. Until then there is nothing to put in the state, and no way to
    /// make an output produce a frame.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if wlroots rejected the commit.
    pub fn commit(&self) -> Result<()> {
        // SAFETY: the handle's lifetime guarantees the output is live.
        // `state` starts uninitialised rather than zeroed: nothing reads it
        // before `wlr_output_state_init` writes it below, so this makes no
        // claim about what bit pattern is valid for `wlr_output_state` (a
        // claim that zeroing would make, and that a future wlroots minor
        // could silently invalidate by adding a field for which zero is
        // not a valid value). `assume_init` is sound because
        // `wlr_output_state_init` fully initialises the value it is handed a
        // pointer to. The state is finished before it drops, as wlroots
        // requires.
        unsafe {
            let mut state = std::mem::MaybeUninit::<sys::wlr_output_state>::uninit();
            sys::wlr_output_state_init(state.as_mut_ptr());
            let mut state = state.assume_init();
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
            // SAFETY: as above — `name` is in bounds of the same allocation.
            // The string is `'static`, so the pointer stays valid for as long
            // as this scratch output can be read, and nothing frees it (real
            // wlroots owns its own `name`, but nothing in this crate frees
            // that either).
            unsafe { (*ptr).name = c"SCRATCH-1".as_ptr().cast_mut() };
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

    /// Exercises `from_raw` against a standalone `wlr_output`, the same style
    /// `id::tests` uses for a standalone `wlr_addon_set`: no display, backend,
    /// or real output is needed to prove the pointer plumbing.
    ///
    /// Asserted through `name()` rather than by comparing a recovered raw
    /// pointer, so that nothing but the public surface has to exist for the
    /// test's sake. It is the stronger check of the two anyway: reading back
    /// the distinctive name this scratch output was given proves the handle
    /// reached *this* struct at the right offset, which a pointer that
    /// `from_raw` merely stored and handed back could not.
    #[test]
    fn from_raw_wraps_the_output_it_was_given() {
        let output = ScratchOutput::new();

        // SAFETY: `output.0` is a live `wlr_output` with an initialised addon
        // set, and the handle does not outlive this function.
        let handle = unsafe { Output::from_raw(output.0) };
        assert_eq!(
            handle.name().as_deref(),
            Some("SCRATCH-1"),
            "the handle must read through to the output it was built from, \
             without copying or offsetting it"
        );
    }

    /// The other half: a null `name` is reported as absent rather than
    /// dereferenced. wlroots leaves it null until the output is configured.
    #[test]
    fn an_unnamed_output_has_no_name() {
        let output = ScratchOutput::new();
        // SAFETY: `output.0` is exclusively owned by this test and live.
        unsafe { (*output.0).name = std::ptr::null_mut() };

        // SAFETY: as in the test above.
        let handle = unsafe { Output::from_raw(output.0) };
        assert_eq!(handle.name(), None);
    }

    /// Pins the panic *message*, not just that `id()` panics: whoever wires
    /// up the real dispatch-time constructor will read this message the
    /// moment they forget to attach an id addon first, so it must name what
    /// is missing rather than just say that `id()` failed.
    #[test]
    #[should_panic(expected = "output has no id addon")]
    fn id_panics_when_no_addon_is_attached() {
        let output = ScratchOutput::new();
        // SAFETY: as in the test above.
        let handle = unsafe { Output::from_raw(output.0) };
        let _ = handle.id();
    }
}
