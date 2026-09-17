//! The `xdg_toplevel_icon_v1` manager and its reference-counted icon object.
//!
//! A client assigns a window icon either by name (an XDG icon-theme stock
//! name) or from pixel data. wlroots forwards the assignment as its `set_icon`
//! signal; unlike every other handle in this crate the icon object is
//! **reference-counted by wlroots** and outlives the client resource that
//! created it, so it is exposed as an owned [`ToplevelIcon`] whose `ref`/`unref`
//! are balanced by [`Clone`]/`Drop`.
//!
//! The manager global is display-owned, so the create call and the pointer the
//! run wiring reads live here; the `set_icon` signal itself is fanned out to
//! [`crate::ToplevelHandler::toplevel_icon_changed`] by `backend.rs`.

use std::ptr::NonNull;

use crate::runtime::copy_nullable_string;
use crate::{Buffer, Display, Error, Result, Runtime, sys};

/// An owned reference to a `wlr_xdg_toplevel_icon_v1`.
///
/// wlroots keeps the icon alive as long as any reference is held, even after
/// the client destroys the resource that created it. This handle holds exactly
/// one such reference: [`Clone`] takes another with
/// `wlr_xdg_toplevel_icon_v1_ref`, and [`Drop`] releases it with
/// `wlr_xdg_toplevel_icon_v1_unref`, which frees the icon when the last
/// reference goes.
pub struct ToplevelIcon {
    raw: NonNull<sys::wlr_xdg_toplevel_icon_v1>,
}

/// Hand-written, for the same reason [`crate::Toplevel`]'s is: the raw pointer
/// is neither useful nor stable. The name is the client-facing identity.
impl std::fmt::Debug for ToplevelIcon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToplevelIcon")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

/// Taking another reference is `Clone` rather than a bare `ref` method because
/// that is what makes two handles safe to hold at once: the two are
/// independent owners and the icon is freed exactly once, when the last drops.
impl Clone for ToplevelIcon {
    fn clone(&self) -> Self {
        // SAFETY: the handle's lifetime guarantees the icon is live, and
        // `wlr_xdg_toplevel_icon_v1_ref` returns the same pointer with one more
        // reference. Reusing `self.raw` rather than rebuilding from the return
        // value keeps this free of any null check: the function cannot fail for
        // a live icon.
        unsafe { sys::wlr_xdg_toplevel_icon_v1_ref(self.raw.as_ptr()) };
        ToplevelIcon { raw: self.raw }
    }
}

/// Releasing the reference is `Drop`, so a handle that escapes a handler is
/// still released exactly once.
impl Drop for ToplevelIcon {
    fn drop(&mut self) {
        // SAFETY: the handle owns one reference to a live icon; unref releases
        // it and frees the icon when it was the last.
        unsafe { sys::wlr_xdg_toplevel_icon_v1_unref(self.raw.as_ptr()) };
    }
}

/// Identity, for the same reason `ToplevelId` is `Eq`: two handles name the
/// same icon iff they wrap the same pointer. Needed because
/// `crate::dispatch::Event` carries an owned icon and derives `PartialEq`.
impl PartialEq for ToplevelIcon {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Eq for ToplevelIcon {}

impl ToplevelIcon {
    /// Wrap a live icon, taking ownership of one existing reference.
    ///
    /// The only producers are `backend.rs`'s `set_icon` callback — which calls
    /// `wlr_xdg_toplevel_icon_v1_ref` on the event's icon before wrapping — and
    /// this module's tests.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `wlr_xdg_toplevel_icon_v1` and the caller must
    /// transfer one reference it holds to the returned handle.
    pub(crate) unsafe fn from_raw(raw: NonNull<sys::wlr_xdg_toplevel_icon_v1>) -> ToplevelIcon {
        ToplevelIcon { raw }
    }

    /// The client's icon name, if it set one.
    ///
    /// This is a stock name from the XDG icon theme, not a file path. `None`
    /// for the pixel-buffer-only form, which is also the form a name-only icon
    /// falls back to when the compositor cannot resolve the name.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        // SAFETY: the handle's lifetime guarantees the icon is live; wlroots
        // leaves `name` null until the client sets one.
        unsafe { copy_nullable_string((*self.raw.as_ptr()).name as *const _) }
    }

    /// The first pixel buffer the client supplied, if any.
    ///
    /// An icon may carry one buffer per scale; wlroots offers no
    /// best-for-scale helper, so this returns the first the client added and
    /// leaves choosing among several to the compositor. `None` for a
    /// name-only icon.
    ///
    /// The returned handle borrows this icon: the client must keep the buffer
    /// alive for as long as the icon is, so it cannot outlive the icon here.
    #[must_use]
    pub fn buffer(&self) -> Option<Buffer<'_>> {
        // SAFETY: the handle's lifetime guarantees the icon is live, so its
        // `buffers` list is an initialised sentinel. Nothing dispatches or
        // frees while the iterator is outstanding: the last entry is turned
        // into a borrowed `Buffer` and the raw pointers never escape.
        // The last entry is the first the client added — wlroots head-inserts
        // each `add_buffer`, so forward iteration runs newest-first.
        unsafe {
            sys::wl_list_for_each!(
                &raw mut (*self.raw.as_ptr()).buffers,
                sys::wlr_xdg_toplevel_icon_v1_buffer,
                link
            )
            .last()
            .and_then(|entry| {
                let buffer = (*entry).buffer;
                NonNull::new(buffer).map(|buffer| Buffer::from_raw(buffer.as_ptr()))
            })
        }
    }
}

impl Runtime {
    /// Create the `xdg_toplevel_icon_v1` global, letting clients assign a
    /// per-toplevel icon. Errors if called twice.
    ///
    /// `version` is the protocol version to advertise; the interface currently
    /// has one version. A client's `set_icon` reaches
    /// [`crate::ToplevelHandler::toplevel_icon_changed`] once a
    /// [`crate::Backend::run_all`](crate::Backend::run_all) has linked the
    /// signal — create the manager before the run.
    pub fn create_xdg_toplevel_icon_manager(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.xdg_toplevel_icon_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_xdg_toplevel_icon_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw =
            unsafe { sys::wlr_xdg_toplevel_icon_manager_v1_create(display.as_ptr(), version) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_xdg_toplevel_icon_manager_v1_create"))?;
        *self.inner.xdg_toplevel_icon_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Declare the icon sizes the compositor prefers, in surface-local pixels.
    ///
    /// wlroots copies the slice and forwards it to bound clients as
    /// `icon_size` events followed by `done`; an empty slice advertises no
    /// preference at all, which the protocol explicitly allows. Errors with
    /// [`Error::Operation`] when no manager was created — sizing a global
    /// that does not exist is a caller bug, not a silent default, the same
    /// double-create guard [`create_xdg_toplevel_icon_manager`](Runtime::create_xdg_toplevel_icon_manager)
    /// applies in the other direction.
    pub fn set_toplevel_icon_sizes(&self, sizes: &[i32]) -> Result<()> {
        let Some(manager) = *self.inner.xdg_toplevel_icon_manager.borrow() else {
            return Err(Error::Operation(
                "Runtime::set_toplevel_icon_sizes with no icon manager",
            ));
        };
        // SAFETY: the manager is live. wlroots copies `n_sizes` ints out of the
        // pointer, so the borrow only has to outlive the call — which it does.
        // The empty slice is passed as null rather than a dangling slice
        // pointer: wlroots returns before reading it, and null makes that
        // impossible to get wrong if it ever stops early-returning.
        let (sizes, n_sizes) = if sizes.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            (sizes.as_ptr().cast_mut(), sizes.len())
        };
        unsafe {
            sys::wlr_xdg_toplevel_icon_manager_v1_set_sizes(manager.as_ptr(), sizes, n_sizes)
        };
        Ok(())
    }

    /// The `xdg_toplevel_icon_manager_v1` manager, once created via
    /// [`Runtime::create_xdg_toplevel_icon_manager`] — read by `backend.rs`'s
    /// `register_toplevel_and_input` to link the `set_icon` listener.
    pub(crate) fn xdg_toplevel_icon_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_xdg_toplevel_icon_manager_v1>> {
        *self.inner.xdg_toplevel_icon_manager.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::ToplevelIcon;
    use crate::sys;
    use std::ffi::c_void;
    use std::ptr::NonNull;

    // Declared directly so no new dependency is needed; std already links libc.
    // `calloc` rather than a Rust allocation because wlroots frees the icon
    // with the C `free`.
    unsafe extern "C" {
        fn calloc(nmemb: usize, size: usize) -> *mut c_void;
        /// `strdup`, so the name wlroots' destroy path `free`s is C-allocated
        /// rather than Rust-allocated — freeing a Rust `CString` with C
        /// `free` would be allocator mismatch.
        fn strdup(s: *const std::ffi::c_char) -> *mut std::ffi::c_char;
    }

    /// A heap icon wlroots is allowed to free, with the empty buffer list its
    /// destroy path walks and one reference already held, standing in for the
    /// reference a client resource owns.
    struct ScratchIcon(*mut sys::wlr_xdg_toplevel_icon_v1);

    impl ScratchIcon {
        fn new() -> Self {
            // SAFETY: `calloc` returns null or a zeroed, suitably aligned block
            // of exactly one icon.
            let raw = unsafe { calloc(1, std::mem::size_of::<sys::wlr_xdg_toplevel_icon_v1>()) }
                .cast::<sys::wlr_xdg_toplevel_icon_v1>();
            assert!(!raw.is_null(), "calloc failed");
            // SAFETY: `raw` is a live, exclusively-owned, zeroed icon. An empty
            // `wl_list` points at itself; the icon is created with one
            // reference held, as wlroots does for a client resource.
            unsafe {
                (*raw).buffers.prev = &raw mut (*raw).buffers;
                (*raw).buffers.next = &raw mut (*raw).buffers;
                (*raw).WLR_PRIVATE.n_refs = 1;
            }
            Self(raw)
        }

        fn refs(&self) -> i32 {
            // SAFETY: `self.0` is live for as long as `self` is; `n_refs` is a
            // plain `c_int` field.
            unsafe { (*self.0).WLR_PRIVATE.n_refs }
        }
    }

    /// wlroots frees the icon, so the scratch owner must not free it too.
    impl Drop for ScratchIcon {
        fn drop(&mut self) {}
    }

    /// `Clone` takes exactly one reference and `Drop` releases exactly one, so
    /// an icon with two handles stays alive until both are gone and is freed
    /// once, on the last drop — the allocator is the double-free oracle.
    #[test]
    fn cloning_and_dropping_balances_the_reference_count() {
        let scratch = ScratchIcon::new();
        assert_eq!(scratch.refs(), 1, "the resource's own reference");
        // SAFETY: `scratch` outlives the handle below.
        let raw = NonNull::new(scratch.0).expect("scratch icon is non-null");
        // SAFETY: `raw` is live with one reference held; the handle takes it.
        let handle = unsafe { ToplevelIcon::from_raw(raw) };
        assert_eq!(scratch.refs(), 1);

        let second = handle.clone();
        assert_eq!(scratch.refs(), 2, "clone took exactly one reference");

        drop(second);
        assert_eq!(scratch.refs(), 1, "the icon is still alive after one drop");

        // The final `unref` frees the scratch, so nothing reads it after this.
        drop(handle);
    }

    /// An icon with no name and no buffers reports both accessors as absent
    /// rather than dereferencing anything — the pre-`set_name`/`add_buffer`
    /// state wlroots hands over.
    #[test]
    fn name_and_buffer_are_none_on_a_bare_icon() {
        let scratch = ScratchIcon::new();
        // SAFETY: `scratch` outlives the handle below.
        let raw = NonNull::new(scratch.0).expect("scratch icon is non-null");
        // SAFETY: `raw` is live with one reference held; the handle takes it.
        let handle = unsafe { ToplevelIcon::from_raw(raw) };
        assert!(handle.name().is_none());
        assert!(handle.buffer().is_none());
        // The handle owns the one reference the scratch stands for; dropping
        // it frees the scratch (whose own `Drop` is the no-op that lets
        // wlroots do that free).
        drop(handle);
    }

    /// A clone names the same icon (`Eq` by pointer), and `Debug` carries the
    /// client-facing name — the two handle conveniences a compositor logging
    /// or comparing icons relies on.
    #[test]
    fn clone_is_equal_and_debug_names_the_icon() {
        let scratch = ScratchIcon::new();
        // SAFETY: `scratch` outlives the handle below; `strdup` returns null
        // (checked) or a C-allocated copy wlroots' destroy path may `free`.
        let name = unsafe { strdup(c"wlr-test-icon".as_ptr()) };
        assert!(!name.is_null(), "strdup failed");
        // SAFETY: `scratch.0` is live and exclusively owned; writing the name
        // field is in bounds.
        unsafe { (*scratch.0).name = name };
        // SAFETY: `scratch` outlives the handle below.
        let raw = NonNull::new(scratch.0).expect("scratch icon is non-null");
        // SAFETY: `raw` is live with one reference held; the handle takes it.
        let handle = unsafe { ToplevelIcon::from_raw(raw) };
        assert_eq!(handle.name().as_deref(), Some("wlr-test-icon"));
        assert_eq!(handle.clone(), handle, "a clone is the same icon");
        let debug = format!("{:?}", handle);
        assert!(
            debug.contains("wlr-test-icon"),
            "Debug names the icon: {debug}"
        );
        // Both drops release one reference each; the last frees the scratch
        // including the `strdup`'d name, which is why the name is C-allocated.
        drop(handle);
    }
}
