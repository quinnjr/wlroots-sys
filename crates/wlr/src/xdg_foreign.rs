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
//!   [`Runtime::find_foreign_exported`] reads one back by handle. A
//!   compositor-created entry withdraws itself when its toplevel is destroyed,
//!   so a lookup can never follow a freed pointer.
//!
//! The protocol objects wlroots creates for clients (`wlr_xdg_exported_v1`,
//! `wlr_xdg_imported_v1` and their v2 siblings) are wlroots-owned and reachable
//! only from inside wlroots' own request handlers; a compositor never holds
//! one, so this crate does not name them.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::cell::Cell;
use std::ffi::CString;
use std::ptr::NonNull;

use crate::backend::{Registration, bound_session, remove_listener};
use crate::id::find_id;
use crate::runtime::copy_nullable_string;
use crate::{Display, Error, Result, Runtime, ToplevelId, sys};

/// The state a [`ForeignExported`]'s watches share with the handle: the entry
/// to withdraw, and whether each owner is still registered.
///
/// Two flags, one per watched signal, because the two owners die
/// independently: the toplevel may die while the entry lives (its callback
/// finishes the entry), or teardown may finish the entry while the toplevel
/// lives. Each [`Registration`] names only its own flag for its `Drop`-time
/// unlink decision, so a dead entry never tricks the toplevel watch into
/// skipping its unlink from a still-live toplevel (which would leave a
/// dangling listener behind), and vice versa. [`ForeignExported::is_alive`]
/// is the AND: the export is registered only while both owners stand.
///
/// Kept in a `Box` on the handle so both the address the toplevel callback
/// receives and the [`Cell`]s the callbacks clear are stable for the
/// registrations' whole life.
struct ExportWatch {
    entry: NonNull<sys::wlr_xdg_foreign_exported>,
    /// `false` once the toplevel's destroy callback ran.
    toplevel_alive: Cell<bool>,
    /// `false` once the entry has been finished — by the toplevel's death, by
    /// the handle's own [`Drop`], or by teardown finishing it underneath us
    /// (registry/display destroy emits the entry's own `destroy`).
    entry_alive: Cell<bool>,
}

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
/// # It withdraws itself when its toplevel dies — or when teardown finishes it
///
/// wlroots' import path follows the base's `toplevel` pointer, so an entry left
/// in the registry after that toplevel is freed would be a use-after-free the
/// moment anyone imported or looked it up. This handle therefore links a
/// listener into the toplevel's `destroy` signal: when the toplevel dies, the
/// entry is finished out of the registry first, and this handle is marked dead.
///
/// It also watches the entry's *own* `destroy` signal (mirroring
/// [`crate::ActivationTokenHandle`'s](crate::ActivationTokenHandle) owner watch):
/// display teardown finishes the entry out of the registry without touching
/// the toplevel, and without this second watch the handle would still read
/// alive and its [`Drop`] would finish an already-finished entry — a double
/// list removal that corrupts the registry's heap list.
/// [`is_alive`](Self::is_alive) reports the AND of both watches, and [`Drop`]
/// then only releases the memory it owns.
pub struct ForeignExported {
    toplevel: ToplevelId,
    /// The toplevel's `destroy` listener, unlinked by its own callback when the
    /// toplevel dies and by this registration's `Drop` otherwise. Declared
    /// before `watch` so it is dropped first — its `Drop` reads
    /// `watch.toplevel_alive`.
    _toplevel_destroy: Registration,
    /// The entry's own `destroy` listener: teardown finishing the entry clears
    /// `watch.entry_alive` and unlinks this, so [`Drop`] knows the entry is
    /// already gone. Declared before `watch` for the same reason; it names
    /// only `watch.entry_alive`, never the toplevel flag, so one owner's death
    /// cannot trick the other registration into skipping its unlink.
    _entry_destroy: Registration,
    watch: Box<ExportWatch>,
}

impl std::fmt::Debug for ForeignExported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The registry handle is a bearer secret: anyone holding it can import
        // the toplevel. Never print it; the explicit `handle()` hand-off path
        // stays available for the out-of-band exchange.
        f.debug_struct("ForeignExported")
            .field("toplevel", &self.toplevel)
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

impl ForeignExported {
    /// The opaque handle clients exchange out of band.
    ///
    /// wlroots generated it at export time and it is a NUL-terminated string in
    /// a fixed 37-byte buffer, readable for as long as this handle owns the
    /// entry. Copied lossily, matching [`Runtime::find_foreign_exported`] and
    /// every other copied C string in this crate: replacing spec-violating
    /// bytes is preferable to rejecting the whole handle.
    ///
    /// Deliberately `None` once dead (like
    /// [`ActivationTokenHandle::name`](crate::ActivationTokenHandle::name)):
    /// the entry memory is still this handle's allocation after withdrawal, so
    /// the bytes could be read — but the handle string no longer resolves, and
    /// handing out a stale bearer secret invites an import the registry will
    /// refuse. Callers that need the last-known handle must copy it while
    /// [`is_alive`](Self::is_alive) holds.
    pub fn handle(&self) -> Option<String> {
        if !self.is_alive() {
            return None;
        }
        // SAFETY: `is_alive` is true, so the entry is still registered and the
        // allocation lives until `Drop`, which runs after every accessor;
        // `handle` is a fixed NUL-terminated `char[37]` wlroots wrote at init.
        // Single policy with every other copy in this crate
        // (`crate::runtime::copy_nullable_string`).
        unsafe {
            let bytes = &(*self.watch.entry.as_ptr()).handle;
            copy_nullable_string(bytes.as_ptr())
        }
    }

    /// The toplevel this export names, while the export is registered.
    ///
    /// `None` once the export has been withdrawn — by the toplevel's death, by
    /// teardown finishing the entry, or (transiently) after this handle's own
    /// `Drop` began — mirroring [`handle`](Self::handle): the stored id would
    /// still be readable, but it no longer names a resolvable export.
    pub fn toplevel_id(&self) -> Option<ToplevelId> {
        if !self.is_alive() {
            return None;
        }
        Some(self.toplevel)
    }

    /// Whether the export is still registered.
    ///
    /// `false` once the toplevel it names has been destroyed — wlroots has
    /// removed the entry from the registry and
    /// [`Runtime::find_foreign_exported`] no longer resolves the handle — or
    /// once teardown has finished the entry underneath a still-live toplevel.
    pub fn is_alive(&self) -> bool {
        self.watch.toplevel_alive.get() && self.watch.entry_alive.get()
    }
}

/// The toplevel an export names is about to be freed.
///
/// Finishes the entry out of the registry *before* the pointer it carries goes
/// stale, then marks the toplevel side dead and unlinks this listener (wlroots
/// commonly asserts the signal is empty after its own destroy). The entry-side
/// watch clears the entry flag itself from inside the `finish` emission, so a
/// teardown that already finished the entry (entry flag clear) skips the
/// second `finish` here rather than removing an unlinked list node twice.
unsafe extern "C" fn on_exported_toplevel_destroy(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `export_foreign` into a live toplevel's `events.destroy`
    // with a `session` pointing at the handle's boxed `ExportWatch`, which
    // outlives the registration. Every call below is infallible and cannot
    // unwind out of this `extern "C"` frame.
    unsafe {
        let session = bound_session(l);
        if session.is_null() {
            return;
        }
        let watch = &*session.cast::<ExportWatch>();
        if watch.toplevel_alive.replace(false) {
            remove_listener(l);
            if watch.entry_alive.get() {
                sys::wlr_xdg_foreign_exported_finish(watch.entry.as_ptr());
            }
        }
    }
}

impl Drop for ForeignExported {
    fn drop(&mut self) {
        if self.is_alive() {
            // SAFETY: both flags are true, so the entry is still registered and
            // this is its sole owner: `finish` emits its `destroy` signal —
            // telling any importer — and unlinks it from the registry list. The
            // emission trips the entry-side watch, which clears `entry_alive`
            // and unlinks itself before returning, so the `_entry_destroy`
            // field drop below correctly skips its own unlink while the
            // toplevel watch (flag still true, toplevel still alive) unlinks
            // normally. The toplevel is still alive for the same reason.
            unsafe { sys::wlr_xdg_foreign_exported_finish(self.watch.entry.as_ptr()) };
        }
        // SAFETY: the entry memory is this crate's allocation whether or not the
        // callback above already finished it, and `Drop` runs once.
        unsafe {
            dealloc(
                self.watch.entry.as_ptr().cast::<u8>(),
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
///
/// New-in-milestone and unreleased: marked [`#[non_exhaustive]`] so future
/// registry fields can be added without breaking downstream construction.
/// Exhaustiveness was never promised for this snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

    /// Link both death watches for a freshly initialised entry.
    ///
    /// Split from [`export_foreign`](Self::export_foreign) so tests can drive
    /// the entry-destroy path with scratch signals through the same linking
    /// code: the toplevel watch finishes a still-registered entry when the
    /// toplevel dies, and the entry watch marks the handle dead when teardown
    /// finishes the entry first. Each registration names only its own flag
    /// (see [`ExportWatch`]), so the returned pair must be stored in
    /// toplevel-then-entry order on a handle that drops both before `watch`.
    ///
    /// # Safety
    ///
    /// * `entry` must be an exclusively-owned entry whose `events.destroy`
    ///   signal is initialised (what `wlr_xdg_foreign_exported_init` leaves
    ///   behind on success).
    /// * `toplevel_destroy` must point at a live toplevel's initialised
    ///   `events.destroy` signal.
    /// * The returned watch (third element) must outlive both registrations:
    ///   the caller stores all three on one handle with the watch last.
    unsafe fn link_export_watches(
        entry: NonNull<sys::wlr_xdg_foreign_exported>,
        toplevel_destroy: *mut sys::wl_signal,
    ) -> (Registration, Registration, Box<ExportWatch>) {
        let watch = Box::new(ExportWatch {
            entry,
            toplevel_alive: Cell::new(true),
            entry_alive: Cell::new(true),
        });
        // Both the watch address and its flags are heap-stable, so the
        // listeners may name them for as long as the registrations live.
        let session: *const () = (&*watch as *const ExportWatch).cast();
        let toplevel_alive: *const Cell<bool> = &watch.toplevel_alive;
        // SAFETY: `toplevel_destroy` is a live toplevel's initialised `destroy`
        // signal per the caller; `watch` (the session and the flag) outlives
        // the registration, which the handle drops before it.
        let destroy = unsafe {
            Registration::link_watched(
                toplevel_destroy,
                on_exported_toplevel_destroy,
                session,
                toplevel_alive,
            )
        };
        let entry_alive: *const Cell<bool> = &watch.entry_alive;
        // SAFETY: `entry` is exclusively owned with an initialised `destroy`
        // signal per the caller, mirroring
        // `ActivationTokenHandle::from_non_null`'s owner watch; `watch`
        // outlives the registration the same way.
        let entry_destroy = unsafe {
            Registration::link_owner_destroy(&raw mut (*entry.as_ptr()).events.destroy, entry_alive)
        };
        (destroy, entry_destroy, watch)
    }

    /// Export a toplevel into the registry and return the owned entry.
    ///
    /// The returned handle owns the entry: dropping it withdraws the export.
    /// The toplevel is recorded both as the entry's target — the
    /// `wlr_xdg_toplevel` pointer wlroots' import path follows — and as the
    /// [`ToplevelId`] the handle reports. The handle also withdraws the entry
    /// automatically if the toplevel is destroyed first, so neither a lookup
    /// nor a later import can follow a freed pointer; see [`ForeignExported`].
    ///
    /// `None` when no registry was created, when `toplevel` names no live
    /// toplevel, or when wlroots could not allocate the entry.
    pub fn export_foreign(&self, toplevel: ToplevelId) -> Option<ForeignExported> {
        let registry = self.foreign_registry_ptr()?;
        let toplevel_entry = self.toplevel_entry(toplevel)?;
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
        // SAFETY: `raw` is exclusively ours and `toplevel_entry.raw` is a live
        // toplevel; the pointer is set for wlroots' import path, exactly as the
        // v1/v2 protocol code sets it. The watch below removes the entry before
        // this pointer can go stale.
        unsafe { (*raw.as_ptr()).toplevel = toplevel_entry.raw.as_ptr() };

        // SAFETY: `toplevel_entry.raw` is a live toplevel with an initialised
        // `destroy` signal, and `raw` is a freshly initialised, exclusively
        // owned entry; the watch box outlives both registrations, which the
        // handle drops before it (see `link_export_watches`).
        let (destroy, entry_destroy, watch) = unsafe {
            Self::link_export_watches(raw, &raw mut (*toplevel_entry.raw.as_ptr()).events.destroy)
        };
        Some(ForeignExported {
            toplevel,
            _toplevel_destroy: destroy,
            _entry_destroy: entry_destroy,
            watch,
        })
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
        // NUL-terminated; the toplevel is dereferenced only to read its addon
        // set. Every entry in the registry has a live toplevel: wlroots removes
        // a client-driven one from the toplevel's own listener and
        // `ForeignExported` removes a compositor-created one from the watch it
        // installs, both before the toplevel is freed.
        unsafe {
            let entry = raw.as_ptr();
            let name = copy_nullable_string((*entry).handle.as_ptr()).unwrap_or_default();
            let toplevel = (*entry).toplevel;
            let toplevel = if toplevel.is_null() {
                None
            } else {
                // `wlr_xdg_foreign_exported.toplevel` names an `xdg_toplevel`;
                // its role id lives on the toplevel's own `wlr_surface`, the
                // same place `backend.rs`'s `toplevel_id_of_surface` reads it.
                // A null `base` is a miss, mirroring the null `toplevel` and
                // null `surface` checks around it: the entry names nothing
                // resolvable, and dereferencing it would be a null read.
                let base = (*toplevel).base;
                if base.is_null() {
                    None
                } else {
                    let surface = (*base).surface;
                    if surface.is_null() {
                        None
                    } else {
                        find_id(&raw const (*surface).addons).map(ToplevelId)
                    }
                }
            };
            Some(ForeignExportInfo {
                handle: name,
                toplevel,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ForeignExported;
    use crate::backend::Registration;
    use crate::{Runtime, ToplevelId, sys};
    use std::alloc::{Layout, alloc_zeroed};
    use std::cell::Cell;
    use std::ptr::NonNull;

    /// A scratch entry plus a scratch stand-in for the toplevel's
    /// `events.destroy`, driving the same [`Runtime::link_export_watches`]
    /// linking code `export_foreign` uses — without a display.
    ///
    /// The entry's `link` is self-pointing (an empty list) and its `destroy`
    /// signal is initialised; the toplevel side is just an initialised
    /// signal. The entry memory is freed by the handle's `Drop`, so this
    /// owner must not free it.
    struct ScratchExport {
        entry: NonNull<sys::wlr_xdg_foreign_exported>,
        toplevel_destroy: Box<sys::wl_signal>,
    }

    impl ScratchExport {
        fn new() -> Self {
            let layout = Layout::new::<sys::wlr_xdg_foreign_exported>();
            // SAFETY: `layout` is non-zero-sized, so `alloc_zeroed` returns
            // null or a suitably aligned, zeroed block for exactly one entry.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_xdg_foreign_exported>();
            let entry = NonNull::new(ptr).expect("scratch entry allocation");
            // SAFETY: `ptr` is a live, exclusively-owned, zeroed entry. An
            // empty `wl_list` points at itself; the `destroy` signal is
            // initialised so watches can link into it.
            unsafe {
                (*ptr).link.prev = &raw mut (*ptr).link;
                (*ptr).link.next = &raw mut (*ptr).link;
                sys::wl_signal_init(&raw mut (*ptr).events.destroy);
            }
            // SAFETY: zeroed then initialised before any listener links in.
            let mut toplevel_destroy: Box<sys::wl_signal> = unsafe { Box::new(std::mem::zeroed()) };
            unsafe { sys::wl_signal_init(&raw mut *toplevel_destroy) };
            Self {
                entry,
                toplevel_destroy,
            }
        }

        /// The production handle for this scratch entry, for tests that drive
        /// one side's destroy first.
        fn handle(&mut self, toplevel: ToplevelId) -> ForeignExported {
            // SAFETY: `entry` is an exclusively-owned entry with an
            // initialised `destroy` signal and `toplevel_destroy` is a live,
            // initialised signal; the handle stores all three together with
            // the watch last, as the contract requires.
            let (destroy, entry_destroy, watch) = unsafe {
                Runtime::link_export_watches(self.entry, &raw mut *self.toplevel_destroy)
            };
            ForeignExported {
                toplevel,
                _toplevel_destroy: destroy,
                _entry_destroy: entry_destroy,
                watch,
            }
        }
    }

    /// Teardown finishing the entry first must mark the export dead, blank its
    /// accessors, and disarm `Drop`'s own `finish`: the emission counter (a
    /// `link_flag` probe reset before the drop) proves no second `finish`
    /// ran. Without the entry-side watch `alive` stays true and `Drop`
    /// removes an already-removed list node — the heap-list corruption this
    /// pins.
    #[test]
    fn teardown_finish_marks_the_export_dead_and_drop_skips_its_own() {
        let mut scratch = ScratchExport::new();
        let id = ToplevelId::dangling_for_test();
        let handle = scratch.handle(id);
        assert!(handle.is_alive(), "a fresh handle owns a live entry");
        assert!(handle.handle().is_some());
        assert_eq!(handle.toplevel_id(), Some(id));

        // A `link_flag` probe counts entry-destroy emissions: it sets the flag
        // on every emission, so resetting it before the drop detects a second
        // `finish`. Linked after the handle (declared after it) so it unlinks
        // while the entry is still alive — before the handle's `Drop` frees it.
        // `link_flag`'s null-`alive` contract needs exactly that ordering.
        let fired = Box::new(Cell::new(false));
        let fired_ptr: *const Cell<bool> = &*fired;
        // SAFETY: the entry signal is initialised and outlives `probe` (the
        // handle below frees it only after `probe` unlinks); `fired` outlives
        // `probe` too.
        let probe = unsafe {
            Registration::link_flag(&raw mut (*scratch.entry.as_ptr()).events.destroy, fired_ptr)
        };

        // Teardown: the registry finishes the entry underneath a live
        // toplevel. `emit_mutable` tolerates the self-unlinking watches, as a
        // real wlroots destroy emission does.
        // SAFETY: the entry signal is initialised with the production watches
        // plus the probe linked in.
        unsafe {
            sys::wl_signal_emit_mutable(
                &raw mut (*scratch.entry.as_ptr()).events.destroy,
                std::ptr::null_mut(),
            )
        };
        assert!(fired.get(), "the teardown emission ran");

        assert!(
            !handle.is_alive(),
            "teardown finishing the entry withdrew the export"
        );
        assert!(
            handle.handle().is_none(),
            "no handle is read off a withdrawn entry"
        );
        assert_eq!(
            handle.toplevel_id(),
            None,
            "no id is reported for a withdrawn entry"
        );

        fired.set(false);
        drop(probe);
        drop(handle);
        assert!(
            !fired.get(),
            "drop after teardown does not finish the entry again"
        );
    }

    /// The toplevel dying first still withdraws the export (finishing the
    /// entry through the production callback) and blanks the accessors: the
    /// pre-existing path the second watch must not disturb.
    #[test]
    fn toplevel_death_still_withdraws_the_export() {
        let mut scratch = ScratchExport::new();
        let id = ToplevelId::dangling_for_test();
        let handle = scratch.handle(id);
        assert!(handle.is_alive());

        // SAFETY: the scratch toplevel signal is initialised with the
        // production watch linked in.
        unsafe {
            sys::wl_signal_emit_mutable(&raw mut *scratch.toplevel_destroy, std::ptr::null_mut())
        };

        assert!(
            !handle.is_alive(),
            "the toplevel's destruction withdrew the export"
        );
        assert!(handle.handle().is_none());
        assert_eq!(handle.toplevel_id(), None);
        // `Drop` sees the same dead flags and only frees the entry memory;
        // under the allocator a double free would be reported.
        drop(handle);
    }
}
