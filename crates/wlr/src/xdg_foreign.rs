//! The `xdg-foreign` family: a registry of exported surfaces plus the v1 and
//! v2 protocol managers that let a client reference another client's toplevel.
//!
//! Out-of-process dialogs need this. A sandboxed client that wants a file
//! chooser to appear above *its* window exports the window: it binds an
//! `xdg_exporter`, names a toplevel, and gets back an opaque handle string. It
//! passes that string out of band (D-Bus, a socket) to a trusted client, which
//! binds an `xdg_importer`, imports the handle, and gets the right to parent
//! its own toplevel to the exported one.
//!
//! The pieces have very different owners:
//!
//! * The [`registry`](Runtime::create_xdg_foreign_registry) is created once and
//!   destroyed with the display. It is the cross-version table both the v1 and
//!   v2 managers share, so an export made through v1 is importable through v2.
//! * The v1 and v2 managers are also display-owned; each advertises an exporter
//!   and an importer global.
//! * A [`ForeignExported`] entry is the *compositor's* to hand out. wlroots
//!   creates one behind the scenes for every client-driven export, and this
//!   crate can also export a toplevel itself with
//!   [`Runtime::export_foreign`]. Both land in the same registry, and
//!   [`Runtime::find_foreign_exported`] reads one back by handle.
//!
//! The protocol objects wlroots creates for clients (`wlr_xdg_exported_v1`,
//! `wlr_xdg_imported_v1` and their v2 siblings) are wlroots-owned and reachable
//! only from inside wlroots' own request handlers; a compositor never holds
//! one, so this crate does not name them.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::{CStr, CString};
use std::ptr::NonNull;

use crate::id::find_id;
use crate::{Display, Error, Result, Runtime, ToplevelId, sys};

/// One surface this compositor has exported, owned by this crate.
///
/// Created by [`Runtime::export_foreign`]; dropping it removes the entry from
/// the registry (wlroots' `wlr_xdg_foreign_exported_finish`), which makes the
/// handle string stop resolving. A client that imported the handle before the
/// drop is told `destroyed` by wlroots.
///
/// The C base this wraps is a plain aggregate, so this crate allocates it and
/// owns the memory as well as the registry membership; both are released in
/// [`Drop`].
///
/// # Keep it alive only while its toplevel is
///
/// wlroots' import path follows the base's `toplevel` pointer, and nothing
/// removes a *compositor-created* entry when that toplevel is destroyed —
/// unlike a client-driven export, whose own listener does. Drop this handle
/// before the toplevel it names; then the entry is gone and no later import can
/// chase the stale pointer. This is the same obligation wlroots' C API leaves
/// with the caller.
pub struct ForeignExported {
    raw: NonNull<sys::wlr_xdg_foreign_exported>,
    toplevel: Option<ToplevelId>,
}

impl std::fmt::Debug for ForeignExported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignExported")
            .field("handle", &self.handle())
            .field("toplevel", &self.toplevel)
            .finish_non_exhaustive()
    }
}

impl ForeignExported {
    /// The opaque handle clients exchange out of band.
    ///
    /// wlroots generated it at export time and it is a NUL-terminated string in
    /// a fixed 37-byte buffer. `None` only for a buffer that is somehow not
    /// terminated, which cannot happen for an entry this crate created.
    pub fn handle(&self) -> Option<String> {
        // SAFETY: the handle owns a live exported entry; `handle` is a fixed
        // NUL-terminated `char[37]` wlroots wrote at init.
        unsafe {
            let bytes = &(*self.raw.as_ptr()).handle;
            CStr::from_ptr(bytes.as_ptr())
                .to_str()
                .ok()
                .map(str::to_owned)
        }
    }

    /// The toplevel this export names, when the compositor supplied one.
    ///
    /// Stored as the stable id at export time, so this never dereferences a
    /// toplevel that may since have been destroyed.
    pub fn toplevel_id(&self) -> Option<ToplevelId> {
        self.toplevel
    }
}

impl Drop for ForeignExported {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of the entry (the constructor's
        // contract) and `Drop` runs once. `finish` emits the entry's `destroy`
        // signal — telling any importer — and unlinks it from the registry
        // list; the memory below is this crate's own allocation.
        unsafe {
            sys::wlr_xdg_foreign_exported_finish(self.raw.as_ptr());
            dealloc(
                self.raw.as_ptr().cast::<u8>(),
                Layout::new::<sys::wlr_xdg_foreign_exported>(),
            );
        }
    }
}

/// A read-only snapshot of a registry entry found by handle.
///
/// Returned by [`Runtime::find_foreign_exported`]. An owned snapshot rather
/// than a handle because a client-driven entry is owned by wlroots: it may be
/// finished by the client, by the exporting toplevel's destruction, or by the
/// display teardown, none of which this crate controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignExportInfo {
    /// The handle string, copied out.
    pub handle: String,
    /// The exported toplevel, when the entry names one this crate tracks.
    pub toplevel: Option<ToplevelId>,
}

impl Runtime {
    /// Create the `xdg-foreign` registry. Errors if called twice.
    ///
    /// The registry is shared by the v1 and v2 managers so an export made
    /// through one version is importable through the other. It lives and dies
    /// with `display`; this crate never frees it.
    pub fn create_xdg_foreign_registry(&self, display: &Display) -> Result<()> {
        if self.inner.xdg_foreign_registry.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_xdg_foreign_registry called twice",
            ));
        }
        // SAFETY: `display` is live for the call; wlroots owns the returned
        // registry and frees it when the display is destroyed.
        let raw = unsafe { sys::wlr_xdg_foreign_registry_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_xdg_foreign_registry_create"))?;
        *self.inner.xdg_foreign_registry.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Create the `zxdg_exporter_v1`/`zxdg_importer_v1` globals against the
    /// registry. Errors if called twice, or before
    /// [`Runtime::create_xdg_foreign_registry`] — the manager needs a registry.
    pub fn create_xdg_foreign_v1(&self, display: &Display) -> Result<()> {
        if self.inner.xdg_foreign_v1.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_xdg_foreign_v1 called twice",
            ));
        }
        let registry = self.foreign_registry_ptr().ok_or(Error::Operation(
            "Runtime::create_xdg_foreign_v1 called before create_xdg_foreign_registry",
        ))?;
        // SAFETY: `display` and `registry` are live; wlroots owns the returned
        // manager and frees it with the display or the registry.
        let raw = unsafe { sys::wlr_xdg_foreign_v1_create(display.as_ptr(), registry.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_xdg_foreign_v1_create"))?;
        *self.inner.xdg_foreign_v1.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Create the `zxdg_exporter_v2`/`zxdg_importer_v2` globals against the
    /// registry. Errors if called twice, or before
    /// [`Runtime::create_xdg_foreign_registry`].
    pub fn create_xdg_foreign_v2(&self, display: &Display) -> Result<()> {
        if self.inner.xdg_foreign_v2.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_xdg_foreign_v2 called twice",
            ));
        }
        let registry = self.foreign_registry_ptr().ok_or(Error::Operation(
            "Runtime::create_xdg_foreign_v2 called before create_xdg_foreign_registry",
        ))?;
        // SAFETY: `display` and `registry` are live; wlroots owns the returned
        // manager and frees it with the display or the registry.
        let raw = unsafe { sys::wlr_xdg_foreign_v2_create(display.as_ptr(), registry.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_xdg_foreign_v2_create"))?;
        *self.inner.xdg_foreign_v2.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The registry pointer, once created.
    pub(crate) fn foreign_registry_ptr(&self) -> Option<NonNull<sys::wlr_xdg_foreign_registry>> {
        *self.inner.xdg_foreign_registry.borrow()
    }

    /// Export a toplevel into the registry and return the owned entry.
    ///
    /// The returned handle owns the entry: dropping it withdraws the export.
    /// `toplevel` is recorded both as the entry's target — the `wlr_xdg_toplevel`
    /// pointer wlroots' import path follows — and as the [`ToplevelId`] the
    /// handle reports, resolved from the runtime's id table at export time.
    /// Because a *compositor-created* entry is not auto-removed when its
    /// toplevel dies, drop the handle before the toplevel is destroyed; see
    /// [`ForeignExported`]'s own doc.
    ///
    /// `None` when no registry was created, when `toplevel` names no live
    /// toplevel, or when wlroots could not allocate the entry.
    pub fn export_foreign(&self, toplevel: Option<ToplevelId>) -> Option<ForeignExported> {
        let registry = self.foreign_registry_ptr()?;
        let layout = Layout::new::<sys::wlr_xdg_foreign_exported>();
        // SAFETY: `layout` is non-zero-sized (the aggregate embeds a list and a
        // signal), so `alloc_zeroed` returns null or a suitably aligned,
        // zeroed block for exactly one entry. `init` below overwrites the link
        // and signal fields it owns.
        let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_xdg_foreign_exported>();
        let raw = NonNull::new(ptr)?;
        // SAFETY: `raw` is a fresh, exclusively-owned, zeroed entry and
        // `registry` is live; `init` generates a unique handle, links the entry
        // in, and initialises its `destroy` signal. It returns false on
        // allocation failure, in which case the block is released below.
        let ok = unsafe { sys::wlr_xdg_foreign_exported_init(raw.as_ptr(), registry.as_ptr()) };
        if !ok {
            // SAFETY: `raw` was allocated above with `layout` and `init` failed
            // before linking it anywhere.
            unsafe { dealloc(raw.as_ptr().cast::<u8>(), layout) };
            return None;
        }
        if let Some(id) = toplevel {
            let Some(entry) = self.toplevel_entry(id) else {
                // No such toplevel: withdraw the just-created entry and report
                // the miss. `finish` unlinks it; then the block is released.
                // SAFETY: the entry was just initialised and is unowned
                // otherwise.
                unsafe {
                    sys::wlr_xdg_foreign_exported_finish(raw.as_ptr());
                    dealloc(raw.as_ptr().cast::<u8>(), layout);
                }
                return None;
            };
            // SAFETY: the entry was just initialised and is exclusively owned by
            // us; `entry.raw` is a live toplevel. wlroots does not read this
            // field back in this crate's paths — it is set for the import
            // side's benefit, exactly as the v1/v2 protocol code sets it.
            unsafe { (*raw.as_ptr()).toplevel = entry.raw.as_ptr() };
        }
        Some(ForeignExported { raw, toplevel })
    }

    /// Look an exported surface up by handle and copy out what it names.
    ///
    /// This is the read side of the registry — the same lookup wlroots performs
    /// when a client imports a handle. `None` when no registry was created,
    /// when `handle` contains an interior NUL or is too long for wlroots, or
    /// when no entry has that handle.
    #[must_use]
    pub fn find_foreign_exported(&self, handle: &str) -> Option<ForeignExportInfo> {
        let registry = self.foreign_registry_ptr()?;
        let handle = CString::new(handle).ok()?;
        // SAFETY: `registry` is live and `handle` is a NUL-terminated string the
        // call only reads; the returned entry is borrowed and every field is
        // copied out before returning.
        let raw = unsafe {
            sys::wlr_xdg_foreign_registry_find_by_handle(registry.as_ptr(), handle.as_ptr())
        };
        let raw = NonNull::new(raw)?;
        // SAFETY: `raw` is a live entry in the registry. The handle string is
        // NUL-terminated; the toplevel, when non-null, is dereferenced only to
        // read its addon set. The registry contract wlroots documents is that an
        // entry is removed before its toplevel is destroyed, so a live entry's
        // toplevel is live.
        unsafe {
            let entry = raw.as_ptr();
            let name = CStr::from_ptr((*entry).handle.as_ptr())
                .to_string_lossy()
                .into_owned();
            let toplevel = (*entry).toplevel;
            let toplevel = if toplevel.is_null() {
                None
            } else {
                // `wlr_xdg_foreign_exported.toplevel` names an `xdg_toplevel`;
                // its role id lives on the toplevel's own `wlr_surface`, the
                // same place `backend.rs`'s `toplevel_id_of_surface` reads it.
                let surface = (*(*toplevel).base).surface;
                if surface.is_null() {
                    None
                } else {
                    find_id(&raw const (*surface).addons).map(ToplevelId)
                }
            };
            Some(ForeignExportInfo {
                handle: name,
                toplevel,
            })
        }
    }
}
