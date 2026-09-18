//! The `xdg_wm_dialog_v1` manager and the per-toplevel dialog role it attaches.
//!
//! A client uses `xdg_wm_dialog_v1` to mark one of its toplevels as a *modal*
//! dialog: a window that should be treated as belonging to a parent and that
//! the compositor may keep stacked above it. wlroots stores the dialog object in
//! an addon on the toplevel's surface, so a compositor reads it by downcasting
//! a live [`Toplevel`] rather than by id.
//!
//! Wrapping is deliberately small: [`Runtime::create_xdg_dialog_manager`]
//! advertises the global, and [`Toplevel::dialog`] is the downcast. The dialog
//! object itself is wlroots-owned and freed with its resource or its toplevel,
//! so [`Dialog`] is borrow-scoped like every other role handle in this crate.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::surface::SurfaceId;
use crate::{Result, Runtime, Toplevel, ToplevelId, sys};

/// A toplevel's `xdg_dialog_v1` role, borrowed for the duration of the
/// [`Toplevel`] handle it was looked up through.
///
/// wlroots frees the dialog when the client destroys the protocol object or
/// the toplevel goes away — neither of which this handle controls — so it is
/// borrow-scoped rather than owned. The toplevel id it names is the storable
/// value.
pub struct Dialog<'h> {
    raw: NonNull<sys::wlr_xdg_dialog_v1>,
    toplevel: ToplevelId,
    _scope: PhantomData<&'h ()>,
}

impl std::fmt::Debug for Dialog<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialog")
            .field("toplevel", &self.toplevel)
            .field("modal", &self.modal())
            .finish_non_exhaustive()
    }
}

impl<'h> Dialog<'h> {
    /// Wrap a live dialog.
    ///
    /// `unwrap`/`assert`-free by construction: the only producer,
    /// [`Toplevel::dialog`], already holds a non-null pointer, so this takes
    /// the `NonNull` rather than re-checking a raw one.
    pub(crate) fn from_non_null(
        raw: NonNull<sys::wlr_xdg_dialog_v1>,
        toplevel: ToplevelId,
    ) -> Dialog<'h> {
        Dialog {
            raw,
            toplevel,
            _scope: PhantomData,
        }
    }

    /// Downcast a live toplevel to its dialog role, if it has one.
    ///
    /// The single `try_from` → null-check → wrap tail shared by
    /// [`Toplevel::dialog`] and [`Runtime::dialog`](crate::Runtime::dialog),
    /// so the unsafe downcast is named once rather than maintained twice.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `wlr_xdg_toplevel`. Only reads.
    pub(crate) unsafe fn from_toplevel_ptr(
        raw: *mut sys::wlr_xdg_toplevel,
        toplevel: ToplevelId,
    ) -> Option<Dialog<'h>> {
        // SAFETY: the caller guarantees `raw` is a live toplevel; the downcast
        // is a read that returns null when no dialog is attached.
        let dialog = unsafe { sys::wlr_xdg_dialog_v1_try_from_wlr_xdg_toplevel(raw) };
        let dialog = NonNull::new(dialog)?;
        Some(Dialog::from_non_null(dialog, toplevel))
    }

    /// Whether the client marked this toplevel as modal.
    ///
    /// The flag is the client's most recent `set_modal`/`unset_modal`, which
    /// wlroots mirrors onto the dialog object; the crate does not layer its own
    /// policy on top.
    #[must_use]
    pub fn modal(&self) -> bool {
        // SAFETY: the handle borrows a live dialog for `'h`; `modal` is a plain
        // bool field.
        unsafe { (*self.raw.as_ptr()).modal }
    }

    /// The id of the toplevel this dialog belongs to.
    #[must_use]
    pub fn toplevel_id(&self) -> ToplevelId {
        self.toplevel
    }
}

impl Toplevel<'_> {
    /// This toplevel's `xdg_dialog_v1` role, if a client created one.
    ///
    /// The counterpart of `wlr_xdg_dialog_v1_try_from_wlr_xdg_toplevel`.
    /// `None` for the overwhelming majority of toplevels — a dialog role exists
    /// only after the client bound `xdg_wm_dialog_v1` and marked this toplevel.
    /// The returned handle borrows this `Toplevel`, so it cannot outlive it.
    #[must_use]
    pub fn dialog(&self) -> Option<Dialog<'_>> {
        // SAFETY: the handle's lifetime guarantees the toplevel is live; see
        // `from_toplevel_ptr` for what the downcast itself needs.
        unsafe { Dialog::from_toplevel_ptr(self.as_ptr(), self.id()) }
    }
}

impl Runtime {
    /// Create the `xdg_wm_dialog_v1` global, letting clients mark a toplevel as
    /// a modal dialog. Errors if called twice.
    ///
    /// `version` is the protocol version to advertise; the interface currently
    /// has one version.
    pub fn create_xdg_dialog_manager(&self, display: &crate::Display, version: u32) -> Result<()> {
        if self.inner.xdg_dialog_manager.borrow().is_some() {
            return Err(crate::Error::Operation(
                "Runtime::create_xdg_dialog_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_xdg_wm_dialog_v1_create(display.as_ptr(), version) };
        let raw = NonNull::new(raw).ok_or(crate::Error::Create("wlr_xdg_wm_dialog_v1_create"))?;
        *self.inner.xdg_dialog_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The [`Toplevel::dialog`] path, resolved from a stored [`ToplevelId`].
    ///
    /// `None` when no live toplevel has `id`, or when that toplevel carries no
    /// dialog role — the by-id miss and the role miss, both explicit. The
    /// returned handle borrows this runtime, the same scope
    /// [`Runtime::tearing_control`](crate::Runtime::tearing_control) gives its
    /// control handle: no event loop can run while the borrow is held, so the
    /// dialog object cannot be freed underneath it, exactly as for that
    /// borrow-scoped role handle.
    #[must_use]
    pub fn dialog(&self, id: ToplevelId) -> Option<Dialog<'_>> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: `entry.raw` is a live toplevel the runtime tracks, and the
        // raw pointer does not escape a bare handle; see `from_toplevel_ptr`.
        unsafe { Dialog::from_toplevel_ptr(entry.raw.as_ptr(), id) }
    }

    /// The dialog role for a surface, when that surface is a tracked toplevel
    /// with a dialog. The [`Toplevel::dialog`] path keyed by surface instead of
    /// toplevel id.
    #[must_use]
    pub fn dialog_of(&self, surface: SurfaceId) -> Option<Dialog<'_>> {
        let id = self.toplevel_of(surface)?.id();
        self.dialog(id)
    }
}

#[cfg(test)]
mod tests {
    use super::Dialog;
    use crate::ToplevelId;
    use crate::sys;
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::ptr::NonNull;

    /// A zeroed `wlr_xdg_dialog_v1` on the heap. Only `modal` is read, so no
    /// other field needs initialising; the block is freed with the same layout.
    struct ScratchDialog(*mut sys::wlr_xdg_dialog_v1);

    impl ScratchDialog {
        fn new(modal: bool) -> Self {
            let layout = Layout::new::<sys::wlr_xdg_dialog_v1>();
            // SAFETY: the struct is non-zero-sized, so `alloc_zeroed` returns
            // null (checked) or a suitably aligned, zeroed allocation.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_xdg_dialog_v1>();
            assert!(!ptr.is_null(), "allocation failed");
            // SAFETY: `ptr` is a fresh, exclusively-owned allocation sized for
            // the dialog; writing the bool is in bounds.
            unsafe { (*ptr).modal = modal };
            Self(ptr)
        }
    }

    impl Drop for ScratchDialog {
        fn drop(&mut self) {
            // SAFETY: allocated with this layout in `new`, and nothing else
            // owns it.
            unsafe { dealloc(self.0.cast::<u8>(), Layout::new::<sys::wlr_xdg_dialog_v1>()) };
        }
    }

    /// The `modal` accessor reads the live flag through the borrow-scoped
    /// handle, and the toplevel id it was built with round-trips. The
    /// integration test in `tests/xdg_protocols.rs` covers the downcast itself.
    #[test]
    fn modal_reads_the_live_flag() {
        for modal in [true, false] {
            let scratch = ScratchDialog::new(modal);
            // SAFETY: `scratch` is non-null and outlives the handle.
            let raw = NonNull::new(scratch.0).expect("scratch dialog is non-null");
            let dialog = Dialog::from_non_null(raw, ToplevelId::dangling_for_test());
            assert_eq!(dialog.modal(), modal);
            assert_eq!(dialog.toplevel_id(), ToplevelId::dangling_for_test());
        }
    }
}
