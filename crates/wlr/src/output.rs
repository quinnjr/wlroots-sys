//! Borrow-scoped output handles.
//!
//! An `Output` is valid only for the handler call that produced it. The lifetime
//! `'h` is what enforces that, and the constructor is `pub(crate)` so a consumer
//! cannot manufacture one with a lifetime of their choosing. A handle that
//! escapes a handler is therefore a compile error, not a documented rule.
//!
//! Anything a consumer needs to remember goes in their own state, keyed by
//! [`OutputId`].

use std::ffi::{CStr, CString};
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::buffer::Buffer;
use crate::geom::{Box2D, FBox, Subpixel, Transform};
use crate::id::{OutputId, find_id};
use crate::region::Region;
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

    /// The output's description, as reported by the backend (or set via
    /// [`set_description`](Output::set_description)).
    pub fn description(&self) -> Option<String> {
        // SAFETY: the handle's lifetime guarantees the output is live.
        // wlroots may leave `description` null before it is set.
        unsafe {
            let desc = (*self.raw.as_ptr()).description;
            if desc.is_null() {
                return None;
            }
            Some(CStr::from_ptr(desc).to_string_lossy().into_owned())
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

    /// The output's current mode size in pixels as `(width, height)`, or
    /// `(0, 0)` before it has one.
    ///
    /// Reads `wlr_output.width`/`height`, which wlroots keeps in step with the
    /// committed mode, so this is the size the scene renders at rather than a
    /// requested one.
    pub fn size(&self) -> (i32, i32) {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe {
            let raw = self.raw.as_ptr();
            ((*raw).width, (*raw).height)
        }
    }

    /// The number of entries the backend's gamma ramp supports, per channel —
    /// `wlr_output_get_gamma_size`. `0` if the backend has no gamma support
    /// at all (the headless backend, for instance, reports `0`).
    ///
    /// A `gamma-control-v1` client normally never needs this read back
    /// directly — wlroots' own scene integration (see
    /// [`crate::Runtime::create_gamma_control_manager`]) handles fitting a
    /// client's ramp to whatever size the backend supports — but a
    /// compositor that wants to know whether an output can do gamma
    /// adjustment at all (to grey out a night-light toggle, say) reads it
    /// here.
    pub fn gamma_size(&self) -> usize {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe { sys::wlr_output_get_gamma_size(self.raw.as_ptr()) }
    }

    /// [`size`](Output::size) with the output's transform applied, as
    /// `(width, height)`.
    ///
    /// A `R90`/`R270`/`Flipped90`/`Flipped270` transform swaps the axes, so
    /// this can differ from `size()` even though no scaling is involved —
    /// `wlr_output_transformed_resolution` is `size()` composed with
    /// [`Transform::apply_coords`](crate::Transform::apply_coords).
    /// Scale is not applied; for that, see `wlr_output_effective_resolution`
    /// (not yet wrapped).
    pub fn transformed_size(&self) -> (i32, i32) {
        let mut width = 0;
        let mut height = 0;
        // SAFETY: the handle's lifetime guarantees the output is live, and
        // both out-parameters point at live locals that outlive the call.
        unsafe {
            sys::wlr_output_transformed_resolution(
                self.raw.as_ptr(),
                &raw mut width,
                &raw mut height,
            );
        }
        (width, height)
    }

    /// Enable the output at its preferred mode and commit.
    ///
    /// The one call that turns an announced output into one that produces
    /// frames. A backend's outputs arrive disabled and modeless; until this
    /// (or an equivalent) runs, `frame` is never emitted and
    /// [`commit`](Output::commit) is observably a no-op.
    ///
    /// "Preferred" is `wlr_output_preferred_mode`, which is the mode wlroots
    /// itself would pick — the backend's native mode on DRM, the window size
    /// under a nested backend, and the configured size for headless. Outputs
    /// with no modes at all (some headless and X11 configurations) are enabled
    /// without a mode, which is correct for them rather than an error.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if wlroots rejected the commit.
    pub fn enable_with_preferred_mode(&self) -> Result<()> {
        // SAFETY: the handle's lifetime guarantees the output is live. The
        // state is initialised before any field is set and finished before it
        // drops, as wlroots requires; `wlr_output_preferred_mode` returns null
        // for a modeless output, which `set_mode` must not be called with.
        unsafe {
            let mut state = std::mem::MaybeUninit::<sys::wlr_output_state>::uninit();
            sys::wlr_output_state_init(state.as_mut_ptr());
            let mut state = state.assume_init();
            sys::wlr_output_state_set_enabled(&raw mut state, true);

            let mode = sys::wlr_output_preferred_mode(self.raw.as_ptr());
            if !mode.is_null() {
                sys::wlr_output_state_set_mode(&raw mut state, mode);
            }

            let ok = sys::wlr_output_commit_state(self.raw.as_ptr(), &raw const state);
            sys::wlr_output_state_finish(&raw mut state);

            if ok {
                Ok(())
            } else {
                Err(Error::Operation("wlr_output_commit_state"))
            }
        }
    }

    /// Ask the backend for one more `frame` event on this output.
    ///
    /// The scene reschedules itself whenever it has new damage, so a
    /// compositor whose content changes never needs this — see
    /// [`Runtime::commit_output`](crate::Runtime::commit_output), which
    /// deliberately does not reschedule on every commit. This is the one-time
    /// kick for an output that has gone idle and needs to repaint anyway:
    /// most notably straight after
    /// [`Runtime::init_output`](crate::Runtime::init_output) on a backend
    /// whose enable commit did not itself produce a frame.
    ///
    /// Infallible because `wlr_output_schedule_frame` returns nothing:
    /// wlroots either has a frame pending already or asks the backend for
    /// one, and reports neither back.
    ///
    /// Safe to call from inside a handler, including
    /// [`OutputHandler::frame`](crate::OutputHandler::frame) — it only marks
    /// the output, it does not dispatch.
    pub fn schedule_frame(&self) {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe { sys::wlr_output_schedule_frame(self.raw.as_ptr()) };
    }

    /// The raw output, for the in-crate callers that pass it to wlroots.
    pub(crate) fn as_ptr(&self) -> *mut sys::wlr_output {
        self.raw.as_ptr()
    }

    /// The output's advertised modes.
    ///
    /// Walks `wlr_output.modes`, a `wl_list` of `wlr_output_mode` linked
    /// through their own `link` field (not nested inside a listener, so the
    /// field path passed to [`wl_list_for_each!`](sys::wl_list_for_each) is
    /// just `link`). Every entry is copied into an owned [`Mode`] before this
    /// returns, so the result stays valid regardless of what happens to the
    /// output afterwards — including a later commit that could in principle
    /// reshuffle the list.
    ///
    /// Some outputs (headless, some nested backends) advertise no modes at
    /// all; those report an empty `Vec` rather than an error, matching
    /// [`enable_with_preferred_mode`](Output::enable_with_preferred_mode)'s
    /// treatment of the same case.
    pub fn modes(&self) -> Vec<Mode> {
        // SAFETY: the handle's lifetime guarantees the output is live, so
        // `modes` is an initialised `wl_list` sentinel head for as long as
        // this call runs. Nothing here dispatches or frees while the
        // iterator is outstanding — each entry is copied into an owned
        // `Mode` and the raw pointer is never retained past this loop body.
        unsafe {
            sys::wl_list_for_each!(
                &raw mut (*self.raw.as_ptr()).modes,
                sys::wlr_output_mode,
                link
            )
            .map(|mode| Mode {
                width: (*mode).width,
                height: (*mode).height,
                refresh_mhz: (*mode).refresh,
                preferred: (*mode).preferred,
            })
            .collect()
        }
    }

    /// Set a custom mode (width, height, refresh in mHz) and commit.
    ///
    /// This is `wlr_output_state_set_custom_mode`, not
    /// `wlr_output_state_set_mode`: it does not pick one of the modes
    /// [`modes`](Output::modes) reports, it asks the backend for an arbitrary
    /// size and refresh rate. On an output that advertises fixed modes (most
    /// real DRM outputs), a custom mode is not guaranteed to work correctly
    /// and may produce visual artifacts — wlroots' own documentation
    /// recommends preferring a listed mode there. It is the right call on
    /// backends that have no fixed mode list, such as headless and most
    /// nested backends, where it is the only way to choose a size at all.
    /// Passing `0` for `refresh_mhz` lets the backend pick a value; the
    /// output must already be enabled.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if wlroots rejected the commit.
    pub fn set_mode(&self, width: i32, height: i32, refresh_mhz: i32) -> Result<()> {
        // SAFETY: the handle's lifetime guarantees the output is live. The
        // state is initialised before any field is set and finished before
        // it drops, as wlroots requires, on every return path.
        unsafe {
            let mut state = std::mem::MaybeUninit::<sys::wlr_output_state>::uninit();
            sys::wlr_output_state_init(state.as_mut_ptr());
            let mut state = state.assume_init();
            sys::wlr_output_state_set_custom_mode(&raw mut state, width, height, refresh_mhz);

            let ok = sys::wlr_output_commit_state(self.raw.as_ptr(), &raw const state);
            sys::wlr_output_state_finish(&raw mut state);

            if ok {
                Ok(())
            } else {
                Err(Error::Operation("wlr_output_commit_state"))
            }
        }
    }

    /// Set the output's scale and commit.
    ///
    /// The scale used to size UI elements up on high-DPI outputs.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if wlroots rejected the commit.
    pub fn set_scale(&self, scale: f32) -> Result<()> {
        // SAFETY: as in `set_mode`.
        unsafe {
            let mut state = std::mem::MaybeUninit::<sys::wlr_output_state>::uninit();
            sys::wlr_output_state_init(state.as_mut_ptr());
            let mut state = state.assume_init();
            sys::wlr_output_state_set_scale(&raw mut state, scale);

            let ok = sys::wlr_output_commit_state(self.raw.as_ptr(), &raw const state);
            sys::wlr_output_state_finish(&raw mut state);

            if ok {
                Ok(())
            } else {
                Err(Error::Operation("wlr_output_commit_state"))
            }
        }
    }

    /// Set the output's transform and commit.
    ///
    /// The transform rotates or flips the output's contents.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if wlroots rejected the commit.
    pub fn set_transform(&self, transform: Transform) -> Result<()> {
        // SAFETY: as in `set_mode`.
        unsafe {
            let mut state = std::mem::MaybeUninit::<sys::wlr_output_state>::uninit();
            sys::wlr_output_state_init(state.as_mut_ptr());
            let mut state = state.assume_init();
            sys::wlr_output_state_set_transform(&raw mut state, transform.into());

            let ok = sys::wlr_output_commit_state(self.raw.as_ptr(), &raw const state);
            sys::wlr_output_state_finish(&raw mut state);

            if ok {
                Ok(())
            } else {
                Err(Error::Operation("wlr_output_commit_state"))
            }
        }
    }

    /// Disable the output and commit.
    ///
    /// The inverse of [`enable_with_preferred_mode`](Output::enable_with_preferred_mode):
    /// after this call the output produces no frames until it is re-enabled.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if wlroots rejected the commit.
    pub fn disable(&self) -> Result<()> {
        // SAFETY: as in `set_mode`.
        unsafe {
            let mut state = std::mem::MaybeUninit::<sys::wlr_output_state>::uninit();
            sys::wlr_output_state_init(state.as_mut_ptr());
            let mut state = state.assume_init();
            sys::wlr_output_state_set_enabled(&raw mut state, false);

            let ok = sys::wlr_output_commit_state(self.raw.as_ptr(), &raw const state);
            sys::wlr_output_state_finish(&raw mut state);

            if ok {
                Ok(())
            } else {
                Err(Error::Operation("wlr_output_commit_state"))
            }
        }
    }

    /// Begin an atomic state transaction on this output.
    ///
    /// Unlike the one-shot setters above ([`set_mode`](Output::set_mode),
    /// [`set_scale`](Output::set_scale), [`set_transform`](Output::set_transform),
    /// [`disable`](Output::disable)), which each commit a single field, a
    /// transaction stages several fields and commits them together: the
    /// display-configuration use case (mode + scale + transform in one
    /// atomic commit). Setters take `&mut self` and never fail — validation
    /// is by type (`Buffer`/`Region` carry non-null pointers; scalars pass
    /// through for wlroots to accept or reject) — and [`commit`](OutputState::commit)
    /// consumes the transaction, which is the single fallible boundary.
    pub fn state(&self) -> OutputState<'_, '_> {
        OutputState::new(self)
    }

    /// Set the output's human-readable name (shown by configuration tools).
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if the name contains an interior NUL (it cannot
    /// cross into C). The underlying call reports no failure of its own.
    pub fn set_name(&self, name: &str) -> Result<()> {
        // SAFETY: the handle's lifetime guarantees the output is live.
        let name = CString::new(name).map_err(|_| Error::Operation("wlr_output_set_name"))?;
        unsafe {
            sys::wlr_output_set_name(self.raw.as_ptr(), name.as_ptr());
        }
        Ok(())
    }

    /// Set the output's human-readable description.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if the description contains an interior NUL.
    /// The underlying call reports no failure of its own.
    pub fn set_description(&self, desc: &str) -> Result<()> {
        // SAFETY: the handle's lifetime guarantees the output is live.
        let desc =
            CString::new(desc).map_err(|_| Error::Operation("wlr_output_set_description"))?;
        unsafe {
            sys::wlr_output_set_description(self.raw.as_ptr(), desc.as_ptr());
        }
        Ok(())
    }

    /// The output's effective resolution in pixels, accounting for scale and
    /// transform — the size its content is actually presented at.
    pub fn effective_resolution(&self) -> (i32, i32) {
        // SAFETY: the handle's lifetime guarantees the output is live. Both
        // out-parameters point at live locals that outlive the call.
        unsafe {
            let mut width = 0;
            let mut height = 0;
            sys::wlr_output_effective_resolution(
                self.raw.as_ptr(),
                &raw mut width,
                &raw mut height,
            );
            (width, height)
        }
    }

    /// Whether this output is driven by the DRM backend.
    ///
    /// Only available with the `drm-backend` feature: without it the
    /// predicate has nothing to ask.
    #[cfg(wlr_has_drm_backend)]
    pub fn is_drm(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe { sys::wlr_output_is_drm(self.raw.as_ptr()) }
    }

    /// Whether this output is driven by the headless backend.
    pub fn is_headless(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe { sys::wlr_output_is_headless(self.raw.as_ptr()) }
    }

    /// Whether this output is driven by the nested Wayland backend.
    pub fn is_wl(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe { sys::wlr_output_is_wl(self.raw.as_ptr()) }
    }

    /// Whether this output is driven by the nested X11 backend.
    ///
    /// Only available with the `x11-backend` feature: without it the
    /// predicate has nothing to ask.
    #[cfg(wlr_has_x11_backend)]
    pub fn is_x11(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe { sys::wlr_output_is_x11(self.raw.as_ptr()) }
    }

    /// Whether direct scan-out is currently allowed on this output.
    /// Typically disallowed while software cursors are forced or during
    /// screen capture.
    pub fn is_direct_scanout_allowed(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe { sys::wlr_output_is_direct_scanout_allowed(self.raw.as_ptr()) }
    }

    /// The output's current adaptive-sync status, read from the live object.
    /// `None` for a value this crate does not know: a future wlroots minor
    /// may add a status, and degrading unknown to `Disabled` would silently
    /// take the sync-off path for the opposite meaning.
    pub fn adaptive_sync_status(&self) -> Option<AdaptiveSyncStatus> {
        // SAFETY: the handle's lifetime guarantees the output is live.
        let raw: sys::wlr_output_adaptive_sync_status =
            unsafe { (*self.raw.as_ptr()).adaptive_sync_status };
        AdaptiveSyncStatus::from_raw(raw.0)
    }

    /// Schedule a `done` event (frame callbacks) on this output.
    pub fn schedule_done(&self) {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe {
            sys::wlr_output_schedule_done(self.raw.as_ptr());
        }
    }

    /// Recompute whether this output needs a frame (damage or timer
    /// pending); use [`needs_frame`](Output::needs_frame) to read the result.
    pub fn update_needs_frame(&self) {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe {
            sys::wlr_output_update_needs_frame(self.raw.as_ptr());
        }
    }

    /// Whether this output currently needs a frame. Recompute first with
    /// [`update_needs_frame`](Output::update_needs_frame) after damage or
    /// timer changes; the flag itself is wlroots-owned.
    pub fn needs_frame(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe { (*self.raw.as_ptr()).needs_frame }
    }

    /// Send a `frame` event: ask clients to draw the next frame.
    pub fn send_frame(&self) {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe {
            sys::wlr_output_send_frame(self.raw.as_ptr());
        }
    }

    /// Send a `present` event built from `event`: report a presented frame
    /// (or its failure) for presentation-time feedback.
    pub fn send_present(&self, event: &PresentEvent) {
        // SAFETY: the handle's lifetime guarantees the output is live. The
        // stack event struct outlives the call; wlroots copies what it keeps.
        unsafe {
            let mut raw = event.as_c(self.raw.as_ptr());
            sys::wlr_output_send_present(self.raw.as_ptr(), &raw mut raw);
        }
    }

    /// Lock the output to rendering instead of direct scan-out (for screen
    /// capture). Every lock needs a matching unlock to restore the original
    /// state; never unlock without a lock.
    pub fn lock_attach_render(&self, lock: bool) {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe {
            sys::wlr_output_lock_attach_render(self.raw.as_ptr(), lock);
        }
    }

    /// Lock the output to software cursors instead of hardware cursors (for
    /// screen capture). Every lock needs a matching unlock.
    pub fn lock_software_cursors(&self, lock: bool) {
        // SAFETY: the handle's lifetime guarantees the output is live.
        unsafe {
            sys::wlr_output_lock_software_cursors(self.raw.as_ptr(), lock);
        }
    }

    /// Create a hardware cursor on this output. The cursor starts invisible
    /// with no buffer; show it with [`OutputCursor::set_buffer`] and place
    /// it with [`OutputCursor::move_to`]. Destroy explicitly with
    /// [`OutputCursor::destroy`] before the output goes away.
    pub fn create_cursor(&self) -> Option<OutputCursor<'_>> {
        // SAFETY: the handle's lifetime guarantees the output is live, which
        // is what the cursor is created on. Null means wlroots refused
        // (allocation failure), mapped to `None`.
        unsafe {
            let raw = sys::wlr_output_cursor_create(self.raw.as_ptr());
            NonNull::new(raw).map(|raw| OutputCursor { raw, staged: None })
        }
    }

    /// Create an output layer on this output: a scene-graph-adjacent plane
    /// the compositor positions itself (drm planes, hardware overlays).
    /// Destroy explicitly with [`OutputLayer::destroy`] before the output
    /// goes away.
    pub fn create_layer(&self) -> Option<OutputLayer> {
        // SAFETY: as in `create_cursor`.
        unsafe {
            let raw = sys::wlr_output_layer_create(self.raw.as_ptr());
            NonNull::new(raw).map(|raw| OutputLayer { raw })
        }
    }
}

/// A hardware cursor owned by its output: created by
/// [`Output::create_cursor`], destroyed explicitly by [`destroy`](OutputCursor::destroy)
/// before the output goes away. Dropping without destroying leaks the C
/// object (deliberately: a `Drop` that frees during wlroots' own teardown
/// ordering would be the use-after-free this discipline avoids).
pub struct OutputCursor<'b> {
    raw: NonNull<sys::wlr_output_cursor>,
    staged: Option<&'b Buffer<'b>>,
}

impl<'b> OutputCursor<'b> {
    /// Destroy the cursor, releasing it. Consumes the handle so a destroyed
    /// cursor cannot be used again.
    pub fn destroy(self) {
        // SAFETY: `raw` names a live cursor: created by `create_cursor`,
        // consumable exactly once by this method, and the output it belongs
        // to outlives the call per the documented discipline.
        unsafe {
            sys::wlr_output_cursor_destroy(self.raw.as_ptr());
        }
    }

    /// Stage a buffer as the cursor image with the given hotspot (the pixel
    /// within the image that sits at the pointer position). The borrow is
    /// stored on the cursor, so the buffer must outlive every later use of
    /// it — the same guard as [`OutputState::set_buffer`].
    pub fn set_buffer(&mut self, buffer: &'b Buffer<'b>, hotspot_x: i32, hotspot_y: i32) {
        // SAFETY: `raw` is live per above; the buffer borrow is stored
        // below, extending its life past this call.
        unsafe {
            sys::wlr_output_cursor_set_buffer(
                self.raw.as_ptr(),
                buffer.as_ptr(),
                hotspot_x,
                hotspot_y,
            );
        }
        self.staged = Some(buffer);
    }

    /// Move the cursor to output-local `(x, y)`, which also places the
    /// hotspot set by the last `set_buffer`. Reports whether wlroots
    /// accepted the move.
    pub fn move_to(&self, x: f64, y: f64) -> bool {
        // SAFETY: `raw` is live per `destroy`'s discipline.
        unsafe { sys::wlr_output_cursor_move(self.raw.as_ptr(), x, y) }
    }

    /// The cursor image size in pixels.
    pub fn size(&self) -> (u32, u32) {
        // SAFETY: `raw` is live per above; read-only scalar fields.
        unsafe {
            let raw = self.raw.as_ptr();
            ((*raw).width, (*raw).height)
        }
    }

    /// Whether the cursor is currently enabled (has a buffer).
    pub fn is_enabled(&self) -> bool {
        // SAFETY: as in `size`.
        unsafe { (*self.raw.as_ptr()).enabled }
    }
}

/// An output layer owned by its output: created by
/// [`Output::create_layer`], destroyed explicitly by [`destroy`](OutputLayer::destroy)
/// before the output goes away. Same owned-handle discipline as
/// [`OutputCursor`].
pub struct OutputLayer {
    raw: NonNull<sys::wlr_output_layer>,
}

impl OutputLayer {
    /// Destroy the layer, releasing it. Consumes the handle.
    pub fn destroy(self) {
        // SAFETY: `raw` names a live layer: created by `create_layer`,
        // consumable exactly once, output discipline as in `OutputCursor`.
        unsafe {
            sys::wlr_output_layer_destroy(self.raw.as_ptr());
        }
    }
}

/// One layer entry of an atomic [`OutputState`]: which layer, what to show
/// in it, and where. All borrows must outlive the transaction's commit —
/// `set_layers` stores them exactly like [`OutputState::set_buffer`]'s
/// guard, so a dropped buffer/region is a compile error.
pub struct LayerState<'l> {
    /// The layer being described.
    pub layer: &'l OutputLayer,
    /// Buffer to show, or `None` to leave the layer's buffer unchanged.
    pub buffer: Option<&'l Buffer<'l>>,
    /// Source box within the buffer.
    pub src: FBox,
    /// Destination box on the output.
    pub dst: Box2D,
    /// Damaged region (buffer-local) for a partial update.
    pub damage: &'l Region,
    /// Whether the compositor accepts this layer.
    pub accepted: bool,
}

/// Adaptive-sync status of an output: whether variable refresh is active.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum AdaptiveSyncStatus {
    /// Adaptive sync is off.
    #[default]
    Disabled = 0,
    /// Adaptive sync is on.
    Enabled = 1,
}

impl AdaptiveSyncStatus {
    /// Decode a raw status value. Unknown values (a future wlroots minor
    /// adding a state) read as [`AdaptiveSyncStatus::Disabled`] rather than
    /// panicking: this is a read-only status, never round-tripped into a
    /// setter, so degrading to off is the safe direction.
    pub fn from_raw(value: u32) -> Option<AdaptiveSyncStatus> {
        Some(match value {
            0 => AdaptiveSyncStatus::Disabled,
            1 => AdaptiveSyncStatus::Enabled,
            _ => return None,
        })
    }
}

/// Present-event flags: how a presented frame reached the screen.
///
/// Same bitmask idiom as [`BufferCaps`](crate::render::BufferCaps): private
/// field with `from_bits`/`bits`/`contains`/`is_empty` and `BitOr`, so
/// unrelated `u32`s cannot mix into a flag set by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PresentFlags(u32);

impl PresentFlags {
    /// Presented on a vertical blank.
    pub const VSYNC: PresentFlags =
        PresentFlags(sys::wlr_output_present_flag::WLR_OUTPUT_PRESENT_VSYNC.0);
    /// Presented with a hardware clock timestamp.
    pub const HW_CLOCK: PresentFlags =
        PresentFlags(sys::wlr_output_present_flag::WLR_OUTPUT_PRESENT_HW_CLOCK.0);
    /// Presented with hardware completion signalling.
    pub const HW_COMPLETION: PresentFlags =
        PresentFlags(sys::wlr_output_present_flag::WLR_OUTPUT_PRESENT_HW_COMPLETION.0);
    /// Presented by zero-copy scan-out.
    pub const ZERO_COPY: PresentFlags =
        PresentFlags(sys::wlr_output_present_flag::WLR_OUTPUT_PRESENT_ZERO_COPY.0);

    /// No flags set.
    pub const NONE: PresentFlags = PresentFlags(0);

    /// Build a set from a raw mask.
    pub fn from_bits(bits: u32) -> PresentFlags {
        PresentFlags(bits)
    }

    /// The raw mask.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Whether every bit of `other` is set here.
    pub fn contains(self, other: PresentFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no bit is set.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for PresentFlags {
    type Output = PresentFlags;

    fn bitor(self, rhs: PresentFlags) -> PresentFlags {
        PresentFlags(self.0 | rhs.0)
    }
}

/// A present event to report via [`Output::send_present`]: which commit is
/// being reported on, whether it presented, when, and with what flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentEvent {
    /// The commit sequence number being reported on.
    pub commit_seq: u32,
    /// Whether the frame presented (false reports a miss).
    pub presented: bool,
    /// When the frame presented, as time since an unspecified epoch (matches
    /// `CLOCK_MONOTONIC` on Linux hosts).
    pub when: std::time::Duration,
    /// Presentation sequence counter.
    pub seq: u32,
    /// Refresh rate in mHz at presentation time.
    pub refresh: i32,
    /// How it reached the screen.
    pub flags: PresentFlags,
}

impl PresentEvent {
    fn as_c(&self, output: *mut sys::wlr_output) -> sys::wlr_output_event_present {
        sys::wlr_output_event_present {
            output,
            commit_seq: self.commit_seq,
            presented: self.presented,
            when: crate::scene::timespec_of(self.when),
            seq: self.seq,
            refresh: self.refresh,
            flags: self.flags.0,
        }
    }
}

/// An atomic state transaction on an output: stage several fields, commit
/// once. Created by [`Output::state`]; [`commit`](OutputState::commit)
/// consumes it. Dropping an uncommitted transaction finishes (abandons)
/// the staged state without committing anything.
///
/// The second lifetime ties a staged buffer to the transaction: `set_buffer`
/// stores the borrow, so the buffer must outlive every later use of the
/// transaction (including `commit`), and dropping the buffer first is a
/// compile error rather than a dangling staged pointer.
pub struct OutputState<'a, 'b> {
    output: &'a Output<'a>,
    state: Option<sys::wlr_output_state>,
    staged_buffer: Option<&'b Buffer<'b>>,
}

impl Drop for OutputState<'_, '_> {
    fn drop(&mut self) {
        if let Some(mut state) = self.state.take() {
            // SAFETY: the state was initialised by `OutputState::new` and
            // has not been finished yet (commit takes it first). Finishing
            // an uncommitted state abandons the staged fields, which is
            // exactly what dropping without committing must do.
            unsafe {
                sys::wlr_output_state_finish(&raw mut state);
            }
        }
    }
}

impl<'a, 'b> OutputState<'a, 'b> {
    fn new(output: &'a Output<'a>) -> Self {
        // SAFETY: `state` starts uninitialised rather than zeroed for the
        // same reason `Output::commit` documents: nothing reads it before
        // `wlr_output_state_init` writes it, and the struct is finished
        // before it drops (here or in `commit`), as wlroots requires.
        let state = unsafe {
            let mut state = std::mem::MaybeUninit::<sys::wlr_output_state>::uninit();
            sys::wlr_output_state_init(state.as_mut_ptr());
            state.assume_init()
        };
        OutputState {
            output,
            state: Some(state),
            staged_buffer: None,
        }
    }

    /// Commit the staged fields atomically.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if wlroots rejected the commit.
    pub fn commit(mut self) -> Result<()> {
        let mut state = self
            .state
            .take()
            .expect("unreachable: commit consumes self");
        // SAFETY: the output handle is live, and the state was initialised
        // by `new`. Finished below on every path, as wlroots requires.
        let ok = unsafe {
            let ok = sys::wlr_output_commit_state(self.output.raw.as_ptr(), &raw const state);
            sys::wlr_output_state_finish(&raw mut state);
            ok
        };
        if ok {
            Ok(())
        } else {
            Err(Error::Operation("wlr_output_commit_state"))
        }
    }

    /// Copy another transaction's staged fields into this one. Reports
    /// whether the copy ran: `false` when either side was already consumed
    /// (committed), so a silently half-copied state can never pass for a
    /// complete one. A staged buffer guard travels with the copy, so the
    /// destination inherits the source's lifetime constraint.
    pub fn copy_from(&mut self, src: &OutputState<'_, 'b>) -> bool {
        // SAFETY: both states are initialised (constructor invariant;
        // committed states are taken, never re-staged). `wlr_output_state_copy`
        // only reads the source.
        unsafe {
            if let (Some(dst), Some(src_state)) = (self.state.as_mut(), src.state.as_ref()) {
                let ok = sys::wlr_output_state_copy(dst as *mut _, src_state as *const _);
                if ok {
                    self.staged_buffer = src.staged_buffer;
                }
                ok
            } else {
                false
            }
        }
    }

    /// The fields staged so far.
    pub fn committed_fields(&self) -> CommittedFields {
        // No unsafe: the state is owned by this transaction, not borrowed
        // C memory; reading its fields touches nothing wlroots owns.
        CommittedFields::from_bits(self.state.as_ref().map(|s| s.committed).unwrap_or(0))
    }

    /// How the staged mode was chosen: a backend mode or a custom size.
    /// `None` when no mode is staged — the raw field reads `Fixed` on a
    /// fresh (zeroed) transaction, which would otherwise fabricate an
    /// answer for "how was the mode chosen" when nothing was.
    pub fn mode_type(&self) -> Option<ModeType> {
        // No unsafe: owned state, read-only scalar field (see
        // `committed_fields`). Names the bindgen newtype so the coverage
        // ledger sees this reader cover `wlr_output_state_mode_type`.
        self.state.as_ref().and_then(|s| {
            if s.committed & CommittedFields::MODE.0 == 0 {
                return None;
            }
            let raw: sys::wlr_output_state_mode_type = s.mode_type;
            ModeType::from_raw(raw.0)
        })
    }

    /// Stage enabled/disabled.
    pub fn set_enabled(&mut self, enabled: bool) {
        // SAFETY: initialised by the constructor; setter borrows nothing.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                sys::wlr_output_state_set_enabled(state as *mut _, enabled);
            }
        }
    }

    /// Stage a custom mode. The output must already be enabled.
    pub fn set_custom_mode(&mut self, width: i32, height: i32, refresh_mhz: i32) {
        // SAFETY: as in `set_enabled`.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                sys::wlr_output_state_set_custom_mode(state as *mut _, width, height, refresh_mhz);
            }
        }
    }

    /// Stage the scale used to size UI elements up on high-DPI outputs.
    pub fn set_scale(&mut self, scale: f32) {
        // SAFETY: as in `set_enabled`.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                sys::wlr_output_state_set_scale(state as *mut _, scale);
            }
        }
    }

    /// Stage the transform (rotation/flip) of the output's contents.
    pub fn set_transform(&mut self, transform: Transform) {
        // SAFETY: as in `set_enabled`.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                sys::wlr_output_state_set_transform(state as *mut _, transform.into());
            }
        }
    }

    /// Stage adaptive sync on or off.
    pub fn set_adaptive_sync_enabled(&mut self, enabled: bool) {
        // SAFETY: as in `set_enabled`.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                sys::wlr_output_state_set_adaptive_sync_enabled(state as *mut _, enabled);
            }
        }
    }

    /// Stage the render format (a DRM format code) for allocated buffers.
    pub fn set_render_format(&mut self, format: u32) {
        // SAFETY: as in `set_enabled`.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                sys::wlr_output_state_set_render_format(state as *mut _, format);
            }
        }
    }

    /// Stage the subpixel geometry of the panel.
    pub fn set_subpixel(&mut self, subpixel: Subpixel) {
        // SAFETY: as in `set_enabled`.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                sys::wlr_output_state_set_subpixel(state as *mut _, subpixel.into());
            }
        }
    }

    /// Stage the buffer to scan out or render from.
    ///
    /// The borrow is stored on the transaction (`staged_buffer`), so the
    /// buffer must outlive every later use of `self`, including
    /// [`commit`](OutputState::commit): dropping the buffer first is a
    /// compile error, not a dangling staged pointer. (The C struct holds
    /// only a borrow; wlroots takes its own reference no earlier than
    /// commit.)
    pub fn set_buffer(&mut self, buffer: &'b Buffer<'b>) {
        // SAFETY: as in `set_enabled`, plus the lifetime above.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                sys::wlr_output_state_set_buffer(state as *mut _, buffer.as_ptr());
            }
        }
        self.staged_buffer = Some(buffer);
    }

    /// Stage the damaged region (buffer-local coordinates) for a partial
    /// update.
    pub fn set_damage(&mut self, region: &Region) {
        // SAFETY: as in `set_enabled`. The region outlives the call;
        // wlroots copies what it keeps.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                sys::wlr_output_state_set_damage(state as *mut _, region.as_ptr());
            }
        }
    }

    /// Stage the output layers for an atomic commit. Each entry borrows its
    /// layer, buffer, and damage region for the transaction's lifetime (same
    /// guard discipline as [`set_buffer`](OutputState::set_buffer)), and the
    /// whole slice must outlive `commit` — dropping any of them first is a
    /// compile error.
    pub fn set_layers<'s, 'l>(&mut self, layers: &'s [LayerState<'l>])
    where
        's: 'b,
        'l: 'b,
    {
        // SAFETY: as in `set_enabled`. The stack array outlives the call;
        // every pointer inside it is either a table-held layer (live while
        // its `OutputLayer` handle stands, which outlives `'l`) or a
        // caller borrow valid for `'b`. wlroots copies the array contents
        // into the state on return.
        unsafe {
            if let Some(state) = self.state.as_mut() {
                let mut raw: Vec<sys::wlr_output_layer_state> = layers
                    .iter()
                    .map(|l| sys::wlr_output_layer_state {
                        layer: l.layer.raw.as_ptr(),
                        buffer: l.buffer.map(|b| b.as_ptr()).unwrap_or(std::ptr::null_mut()),
                        src_box: sys::wlr_fbox {
                            x: l.src.x,
                            y: l.src.y,
                            width: l.src.width,
                            height: l.src.height,
                        },
                        dst_box: sys::wlr_box {
                            x: l.dst.x,
                            y: l.dst.y,
                            width: l.dst.width,
                            height: l.dst.height,
                        },
                        damage: l.damage.as_ptr(),
                        accepted: l.accepted,
                    })
                    .collect();
                sys::wlr_output_state_set_layers(state as *mut _, raw.as_mut_ptr(), raw.len());
            }
        }
        // No stored guard is needed beyond the signature above: `&'s [..]`
        // with `'s: 'b, 'l: 'b` already forces every layer, buffer, and
        // region borrow to outlive every later use of `self`, exactly like
        // the stored `staged_buffer` guard does for `set_buffer`.
    }
}

/// Which fields an [`OutputState`] transaction has staged: the
/// `wlr_output_state_field` bitmask in a typed wrapper, so staged-field
/// sets cannot mix with unrelated `u32`s (refresh rates, formats, commit
/// seqs). Same bitmask idiom as [`BufferCaps`](crate::render::BufferCaps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CommittedFields(u32);

impl CommittedFields {
    /// A buffer is staged.
    pub const BUFFER: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_BUFFER.0);
    /// Damage is staged.
    pub const DAMAGE: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_DAMAGE.0);
    /// A mode is staged.
    pub const MODE: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_MODE.0);
    /// The enabled flag is staged.
    pub const ENABLED: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_ENABLED.0);
    /// The output scale is staged.
    pub const SCALE: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_SCALE.0);
    /// A transform is staged.
    pub const TRANSFORM: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_TRANSFORM.0);
    /// Adaptive sync is staged.
    pub const ADAPTIVE_SYNC_ENABLED: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_ADAPTIVE_SYNC_ENABLED.0);
    /// A render format is staged.
    pub const RENDER_FORMAT: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_RENDER_FORMAT.0);
    /// A subpixel geometry is staged.
    pub const SUBPIXEL: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_SUBPIXEL.0);
    /// Whether output layers are staged.
    pub const LAYERS: CommittedFields =
        CommittedFields(sys::wlr_output_state_field::WLR_OUTPUT_STATE_LAYERS.0);

    /// No fields staged.
    pub const NONE: CommittedFields = CommittedFields(0);

    /// Build a set from a raw mask.
    pub fn from_bits(bits: u32) -> CommittedFields {
        CommittedFields(bits)
    }

    /// The raw mask.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Whether every bit of `other` is set here.
    pub fn contains(self, other: CommittedFields) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no bit is set.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for CommittedFields {
    type Output = CommittedFields;

    fn bitor(self, rhs: CommittedFields) -> CommittedFields {
        CommittedFields(self.0 | rhs.0)
    }
}

/// How a staged mode was chosen.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ModeType {
    /// A backend-advertised mode.
    Fixed = 0,
    /// A custom width/height/refresh.
    Custom = 1,
}

impl ModeType {
    /// Decode a raw mode-type value. `None` for anything outside 0..=1.
    pub fn from_raw(value: u32) -> Option<ModeType> {
        Some(match value {
            0 => ModeType::Fixed,
            1 => ModeType::Custom,
            _ => return None,
        })
    }
}

/// A single mode an output advertises: a size and refresh rate the backend
/// can drive it at.
///
/// Returned by [`Output::modes`]. Owned and copied out of wlroots' own list,
/// so it stays valid independent of the output's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Refresh rate in mHz (millihertz) — divide by 1000 for Hz.
    pub refresh_mhz: i32,
    /// Whether the backend advertises this as its preferred mode. At most
    /// one mode in a given [`Output::modes`] list normally reports `true`.
    pub preferred: bool,
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

    /// `size` reads through to the struct rather than reporting a constant.
    #[test]
    fn size_reports_the_outputs_own_dimensions() {
        let output = ScratchOutput::new();
        // SAFETY: `output.0` is exclusively owned by this test and live.
        unsafe {
            (*output.0).width = 1280;
            (*output.0).height = 720;
        }
        // SAFETY: as in the tests above.
        let handle = unsafe { Output::from_raw(output.0) };
        assert_eq!(handle.size(), (1280, 720));
    }

    /// A quarter-turn transform swaps width and height without scaling.
    #[test]
    fn transformed_size_swaps_axes_under_a_quarter_turn() {
        let output = ScratchOutput::new();
        // SAFETY: `output.0` is exclusively owned by this test and live.
        unsafe {
            (*output.0).width = 1280;
            (*output.0).height = 720;
            (*output.0).transform = sys::wl_output_transform::WL_OUTPUT_TRANSFORM_90;
        }
        // SAFETY: as in the tests above.
        let handle = unsafe { Output::from_raw(output.0) };
        assert_eq!(handle.transformed_size(), (720, 1280));
    }

    /// `Normal` is a no-op, matching [`Output::size`] exactly.
    #[test]
    fn transformed_size_matches_size_under_the_normal_transform() {
        let output = ScratchOutput::new();
        // SAFETY: `output.0` is exclusively owned by this test and live.
        unsafe {
            (*output.0).width = 1280;
            (*output.0).height = 720;
            (*output.0).transform = sys::wl_output_transform::WL_OUTPUT_TRANSFORM_NORMAL;
        }
        // SAFETY: as in the tests above.
        let handle = unsafe { Output::from_raw(output.0) };
        assert_eq!(handle.transformed_size(), handle.size());
    }

    /// `modes` walks `wlr_output.modes` and copies each entry out as an
    /// owned [`Mode`].
    ///
    /// The list head lives inside `ScratchOutput`'s zeroed allocation, so it
    /// is initialised explicitly here rather than relying on any zero bit
    /// pattern being a valid empty `wl_list` (it happens not to be: an empty
    /// `wl_list`'s `next`/`prev` must point at the head itself, which zeroed
    /// null pointers do not). One `wlr_output_mode` is allocated separately
    /// on the heap and linked in, then freed after the assertions run.
    ///
    /// Mutation-covering: flipping any field copied in `modes` (width,
    /// height, `refresh` vs `refresh_mhz`, `preferred`), returning an empty
    /// `Vec` unconditionally, or getting the `link` field offset wrong (which
    /// would panic or read garbage rather than the mode's real fields) fails
    /// this assertion.
    #[test]
    fn modes_reports_the_outputs_linked_modes() {
        let output = ScratchOutput::new();
        // SAFETY: `output.0` is exclusively owned by this test and live; its
        // `modes` field is uninitialised (from `alloc_zeroed`, not a valid
        // `wl_list`) until `wl_list_init` runs here.
        unsafe { sys::wayland_sys::server::wl_list_init(&raw mut (*output.0).modes) };

        let mode_layout = Layout::new::<sys::wlr_output_mode>();
        // SAFETY: `mode_layout` is non-zero-sized, so `alloc_zeroed` returns
        // either null (checked below) or a suitably aligned, zeroed
        // allocation of exactly that size.
        let mode = unsafe { alloc_zeroed(mode_layout) }.cast::<sys::wlr_output_mode>();
        assert!(!mode.is_null(), "allocation failed");
        // SAFETY: `mode` is a fresh, exclusively-owned allocation sized for
        // `wlr_output_mode`; these writes are all in bounds of it, and
        // `wl_list_insert` only touches the `link` field's own `wl_list`
        // plus the (now-initialised) list head, both live for the duration
        // of this test.
        unsafe {
            (*mode).width = 1920;
            (*mode).height = 1080;
            (*mode).refresh = 60_000;
            (*mode).preferred = true;
            sys::wayland_sys::server::wl_list_insert(
                &raw mut (*output.0).modes,
                &raw mut (*mode).link,
            );
        }

        // SAFETY: `output.0` is live and its `modes` list is initialised and
        // has exactly the one entry linked above.
        let handle = unsafe { Output::from_raw(output.0) };
        let modes = handle.modes();

        // SAFETY: `mode` is not used again after this point; `link` was
        // never removed from the list, but the list head itself (part of
        // `output.0`'s allocation) is torn down independently in
        // `ScratchOutput::drop`, so this free does not leave a dangling
        // entry any later code walks.
        unsafe { dealloc(mode.cast::<u8>(), mode_layout) };

        assert_eq!(
            modes,
            vec![Mode {
                width: 1920,
                height: 1080,
                refresh_mhz: 60_000,
                preferred: true,
            }],
            "modes() must report the one linked wlr_output_mode with its \
             real fields, not an empty or wrong list"
        );
    }

    /// Discriminants pinned against the headers, not the comments: a swapped
    /// constant would read every mode/adaptive decision backwards silently.
    #[test]
    fn mode_type_and_adaptive_sync_match_wlroots() {
        assert_eq!(
            ModeType::from_raw(sys::wlr_output_state_mode_type::WLR_OUTPUT_STATE_MODE_FIXED.0),
            Some(ModeType::Fixed)
        );
        assert_eq!(
            ModeType::from_raw(sys::wlr_output_state_mode_type::WLR_OUTPUT_STATE_MODE_CUSTOM.0),
            Some(ModeType::Custom)
        );
        assert_eq!(ModeType::from_raw(2), None);
        assert_eq!(ModeType::from_raw(u32::MAX), None);
        assert_eq!(
            AdaptiveSyncStatus::from_raw(
                sys::wlr_output_adaptive_sync_status::WLR_OUTPUT_ADAPTIVE_SYNC_DISABLED.0
            ),
            Some(AdaptiveSyncStatus::Disabled)
        );
        assert_eq!(
            AdaptiveSyncStatus::from_raw(
                sys::wlr_output_adaptive_sync_status::WLR_OUTPUT_ADAPTIVE_SYNC_ENABLED.0
            ),
            Some(AdaptiveSyncStatus::Enabled)
        );
        assert_eq!(AdaptiveSyncStatus::from_raw(99), None);
    }

    /// Bitmask values pinned against the headers: a swapped discriminant
    /// would read every present/field decision backwards silently.
    #[test]
    fn present_and_field_bits_match_wlroots() {
        for (ours, theirs) in [
            (
                PresentFlags::VSYNC.bits(),
                sys::wlr_output_present_flag::WLR_OUTPUT_PRESENT_VSYNC.0,
            ),
            (
                PresentFlags::HW_CLOCK.bits(),
                sys::wlr_output_present_flag::WLR_OUTPUT_PRESENT_HW_CLOCK.0,
            ),
            (
                PresentFlags::HW_COMPLETION.bits(),
                sys::wlr_output_present_flag::WLR_OUTPUT_PRESENT_HW_COMPLETION.0,
            ),
            (
                PresentFlags::ZERO_COPY.bits(),
                sys::wlr_output_present_flag::WLR_OUTPUT_PRESENT_ZERO_COPY.0,
            ),
        ] {
            assert_eq!(ours, theirs);
        }
        for (ours, theirs) in [
            (
                CommittedFields::BUFFER.bits(),
                sys::wlr_output_state_field::WLR_OUTPUT_STATE_BUFFER.0,
            ),
            (
                CommittedFields::DAMAGE.bits(),
                sys::wlr_output_state_field::WLR_OUTPUT_STATE_DAMAGE.0,
            ),
            (
                CommittedFields::MODE.bits(),
                sys::wlr_output_state_field::WLR_OUTPUT_STATE_MODE.0,
            ),
            (
                CommittedFields::ENABLED.bits(),
                sys::wlr_output_state_field::WLR_OUTPUT_STATE_ENABLED.0,
            ),
            (
                CommittedFields::SCALE.bits(),
                sys::wlr_output_state_field::WLR_OUTPUT_STATE_SCALE.0,
            ),
            (
                CommittedFields::TRANSFORM.bits(),
                sys::wlr_output_state_field::WLR_OUTPUT_STATE_TRANSFORM.0,
            ),
            (
                CommittedFields::ADAPTIVE_SYNC_ENABLED.bits(),
                sys::wlr_output_state_field::WLR_OUTPUT_STATE_ADAPTIVE_SYNC_ENABLED.0,
            ),
            (
                CommittedFields::RENDER_FORMAT.bits(),
                sys::wlr_output_state_field::WLR_OUTPUT_STATE_RENDER_FORMAT.0,
            ),
            (
                CommittedFields::SUBPIXEL.bits(),
                sys::wlr_output_state_field::WLR_OUTPUT_STATE_SUBPIXEL.0,
            ),
        ] {
            assert_eq!(ours, theirs);
        }
        assert!(CommittedFields::NONE.is_empty());
        assert!(!CommittedFields::SCALE.is_empty());
        assert!(CommittedFields::SCALE.contains(CommittedFields::SCALE));
        assert!(!CommittedFields::SCALE.contains(CommittedFields::MODE));
    }

    /// The LAYERS bit pinned against the header like every other staged
    /// bit: a swapped discriminant would misreport layer staging silently.
    #[test]
    fn layers_bit_matches_wlroots() {
        assert_eq!(
            CommittedFields::LAYERS.bits(),
            sys::wlr_output_state_field::WLR_OUTPUT_STATE_LAYERS.0
        );
        assert!(!CommittedFields::LAYERS.is_empty());
        assert!(!CommittedFields::LAYERS.contains(CommittedFields::MODE));
    }

    /// Flag combinators behave as a set algebra; unknown bits round-trip
    /// losslessly for forward compatibility.
    #[test]
    fn present_flags_compose_and_query() {
        let both = PresentFlags::VSYNC | PresentFlags::ZERO_COPY;
        assert!(both.contains(PresentFlags::VSYNC));
        assert!(both.contains(PresentFlags::ZERO_COPY));
        assert!(!both.contains(PresentFlags::HW_CLOCK));
        assert!(!both.is_empty());
        assert!(PresentFlags::NONE.is_empty());
        assert_eq!(
            PresentFlags::from_bits(0xFFFF).bits(),
            0xFFFF,
            "unknown future bits must survive the round trip"
        );
    }
}
