//! `#[cfg(test)]`-only support shared by this crate's unit tests.
//!
//! Gated behind `#[cfg(test)]` in `lib.rs`, so nothing here is compiled into,
//! or reachable from, the published library: `wlr`'s public surface is
//! unchanged by this module's existence.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::surface::{Surface, SurfaceId};
use crate::sys;

static DISPLAY_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

/// Serializes `Display`/`Backend` bring-up across the tests in this crate's
/// `--lib` unit-test binary.
///
/// libwayland-server holds process-global state, and libtest runs `#[test]`
/// functions on parallel threads by default, so two `Display::new` calls
/// racing on different threads abort with `data is non-NULL with zero alloc`.
/// Hold this for the whole test body of every unit test that creates a
/// `Display` or a `Backend` — the unit-test counterpart of the integration
/// tests' `common::headless_guard`, and poison-tolerant for the same reason: a
/// test that panicked while holding it must not poison every later test in the
/// binary.
///
/// Must stay in lockstep with
/// `crates/wlr/tests/common/mod.rs`'s `headless_guard`: the two cannot share
/// an implementation across the `cfg(test)` lib / integration-crate boundary,
/// so both copies are poison-tolerant for the same reason.
pub(crate) fn test_display_guard() -> MutexGuard<'static, ()> {
    DISPLAY_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A zeroed, heap-allocated `wlr_surface` with an initialised (empty) addon set.
///
/// Shared by the unit tests that need a live `wlr_surface` without a display:
/// reading its plain fields needs only zeroed memory, and initialising the addon
/// set makes `wlr_addon_find`-backed lookups (`find_surface_id`) safe too. Uses
/// `alloc_zeroed` rather than `std::mem::zeroed`, for the same reason the
/// original copies did: `wlr_surface` embeds `wl_signal`/`wl_listener`
/// machinery with bare function pointers, a bit pattern `std::mem::zeroed`
/// refuses to produce as a materialised *value*; touching the bytes only
/// through a raw pointer sidesteps that.
pub(crate) struct ScratchSurface {
    /// The allocation. Public within the crate so a test can hand the raw
    /// pointer to code under test or read plain fields through it.
    pub(crate) raw: *mut sys::wlr_surface,
}

impl ScratchSurface {
    /// Allocate and initialise one.
    pub(crate) fn new() -> Self {
        let layout = Layout::new::<sys::wlr_surface>();
        // SAFETY: `wlr_surface` is non-zero-sized, so `alloc_zeroed` returns
        // either null (checked below) or a suitably aligned, zeroed allocation
        // of exactly that size.
        let raw = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_surface>();
        assert!(!raw.is_null(), "allocation failed");
        // SAFETY: `raw` is sized for the addon set it embeds.
        unsafe { sys::wlr_addon_set_init(&raw mut (*raw).addons) };
        Self { raw }
    }

    /// As [`new`](Self::new), plus initialised `events` signals.
    ///
    /// For the tests that link listeners and emit signals on the scratch
    /// surface (`backend.rs`'s generic-surface tests): without initialised
    /// heads `wl_signal_add` would corrupt the heap through the zeroed list
    /// pointers. Tests that only read plain fields or the addon set use
    /// `new` and pay for nothing they don't touch.
    pub(crate) fn new_with_signals() -> Self {
        let this = Self::new();
        // SAFETY: `this.raw` is a fresh, exclusively-owned allocation sized
        // for a whole `wlr_surface`, so every signal written below is in
        // bounds; each initialiser writes only the `wl_list` head it owns.
        // The allocation does not move again, which matters because
        // `wl_signal_init` makes each head point at itself.
        unsafe {
            sys::wl_signal_init(&raw mut (*this.raw).events.commit);
            sys::wl_signal_init(&raw mut (*this.raw).events.map);
            sys::wl_signal_init(&raw mut (*this.raw).events.unmap);
            sys::wl_signal_init(&raw mut (*this.raw).events.destroy);
            sys::wl_signal_init(&raw mut (*this.raw).events.new_subsurface);
        }
        this
    }

    /// # Safety
    ///
    /// The returned handle borrows this `ScratchSurface`'s allocation and must
    /// not outlive it.
    pub(crate) unsafe fn surface(&self, id: SurfaceId) -> Surface<'_> {
        // SAFETY: the caller upholds the lifetime bound.
        unsafe { Surface::from_raw_with_id(self.raw, id) }
    }
}

impl Drop for ScratchSurface {
    fn drop(&mut self) {
        // SAFETY: `raw` was allocated by `alloc_zeroed` with the matching
        // layout, is still exclusively owned, and nothing else frees or aliases
        // it. The addon set was initialised in `new`; finishing it runs any
        // destroy hooks for addons a test attached and frees them.
        unsafe {
            sys::wlr_addon_set_finish(&raw mut (*self.raw).addons);
            dealloc(self.raw.cast::<u8>(), Layout::new::<sys::wlr_surface>());
        }
    }
}

/// A zeroed heap allocation of one `T`, with caller-supplied init and fini.
///
/// The alloc-zero-drop trio the `tearing.rs` manager/control scratch objects
/// share: `alloc_zeroed` rather than `std::mem::zeroed` for the reason
/// [`ScratchSurface`] documents, `init` runs while the allocation is
/// exclusively owned, and `Drop` runs `fini` before freeing. Helpers with
/// bespoke ownership — a `calloc` wlroots frees itself, an addon set to
/// finish — keep their own type instead of bending this one around them.
pub(crate) struct Scratch<T> {
    /// The allocation. Public within the crate so a test can hand the raw
    /// pointer to code under test or read plain fields through it.
    pub(crate) ptr: *mut T,
    fini: fn(*mut T),
}

impl<T> Scratch<T> {
    /// Allocate, zero, and initialise one.
    ///
    /// `init` sees the allocation exclusively owned and zeroed; `fini` must
    /// undo exactly what `init` did and touch nothing else.
    pub(crate) fn new(init: impl FnOnce(*mut T), fini: fn(*mut T)) -> Self {
        let layout = Layout::new::<T>();
        // SAFETY: `T` is a non-zero-sized C struct in every use, so
        // `alloc_zeroed` returns either null (checked below) or a suitably
        // aligned, zeroed allocation of exactly that size.
        let ptr = unsafe { alloc_zeroed(layout) }.cast::<T>();
        assert!(!ptr.is_null(), "allocation failed");
        init(ptr);
        Self { ptr, fini }
    }
}

impl<T> Drop for Scratch<T> {
    fn drop(&mut self) {
        (self.fini)(self.ptr);
        // SAFETY: `ptr` was allocated by `alloc_zeroed` with the matching
        // layout, is still exclusively owned, and nothing else frees it.
        unsafe { dealloc(self.ptr.cast::<u8>(), Layout::new::<T>()) };
    }
}
