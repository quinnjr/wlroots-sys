//! DRM synchronisation-object timelines: explicit GPU synchronisation.
//!
//! A timeline is a monotonically increasing sequence of 64-bit points, modelled
//! on Vulkan timeline semaphores and backed by a kernel `drm_syncobj`. A
//! producer signals a point when it has finished writing a buffer; a consumer
//! waits on that point before reading. [`RenderPass`](super::RenderPass) can do
//! both — see [`BufferPassOptions::signal`](super::BufferPassOptions::signal)
//! and [`TextureOptions::wait`](super::TextureOptions::wait).
//!
//! # The descriptor is retained, and this crate cannot enforce that
//!
//! `wlr_drm_syncobj_timeline_create` stores the DRM file descriptor it is given
//! and uses it for every later ioctl, destruction included
//! (`render/drm_syncobj.c`). It does **not** dup it. So the descriptor must
//! outlive the timeline, and unlike everything else in this module that is a
//! documented rule rather than a borrow: [`SyncTimeline`] carries no lifetime.
//!
//! That is deliberate. wlroots' own use of timelines — `linux-drm-syncobj-v1`
//! stores them on surface state, passes name them, buffers hold them — is the
//! reason they are reference-counted at all, and a lifetime tying one to the
//! [`Renderer`](crate::Renderer) whose `drm_fd()` it came from would make
//! storing a timeline beside the renderer a self-referential struct. The
//! descriptor a compositor actually uses here is the renderer's, which lives
//! for the renderer's whole life; [`SyncTimeline::drm_fd`] reports which one a
//! timeline is on, and [`SyncTimeline::transfer`] uses it to refuse a pairing
//! wlroots would abort on.
//!
//! # Waiters are pinned, and finishing is mandatory
//!
//! `wlr_drm_syncobj_timeline_waiter` is caller-allocated and wlroots stores its
//! address in a libwayland event source, so [`SyncWaiter`] boxes it and keeps
//! the C struct as its first field.
//!
//! `wlr_drm_syncobj_timeline_waiter_finish` is legal **after** the callback has
//! fired, and in fact required: the callback path
//! (`handle_eventfd_ready` in `render/drm_syncobj.c`) reads the eventfd, calls
//! the callback and returns — it removes no event source and closes no
//! descriptor. Verified in wlroots 0.20.2's source rather than inferred from
//! the header's word "cancel", because getting it wrong in either direction is
//! a leak or a double free. [`SyncWaiter`]'s `Drop` therefore calls `finish`
//! unconditionally, and no fired-flag is needed.

use std::cell::Cell;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

use crate::display::EventLoop;
use crate::error::{Error, Result};
use crate::sys;

/// How a wait on a timeline point behaves.
///
/// These are libdrm's `DRM_SYNCOBJ_WAIT_FLAGS_*` from `<drm.h>`, not wlroots
/// symbols, so they are not in `wlr-sys` — its allowlist is `wlr_.*`/`WLR_.*`
/// and adding libdrm to it for two integers would pull a second foreign ABI
/// into the bindings. They are written out here and pinned against the
/// installed `drm.h` by `tests/render_sync.rs`.
///
/// The empty set is the default: a bare "is this point already signalled?".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SyncFlags(u32);

impl SyncFlags {
    /// The empty set — succeed only if the point is already signalled.
    pub const NONE: SyncFlags = SyncFlags(0);

    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT`: treat a point with no fence
    /// yet as not-yet-signalled instead of failing.
    pub const WAIT_FOR_SUBMIT: SyncFlags = SyncFlags(1 << 1);

    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE`: succeed as soon as a fence has
    /// materialised for the point, without waiting for it to signal.
    ///
    /// The only flag the kernel accepts for
    /// [`EventLoop::wait_for_timeline`] — `DRM_IOCTL_SYNCOBJ_EVENTFD` rejects
    /// every other bit with `EINVAL`.
    pub const WAIT_AVAILABLE: SyncFlags = SyncFlags(1 << 2);

    /// Build a set from a raw `DRM_SYNCOBJ_WAIT_FLAGS_*` mask.
    pub fn from_bits(bits: u32) -> SyncFlags {
        SyncFlags(bits)
    }

    /// The raw mask.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Whether every bit of `other` is set here.
    pub fn contains(self, other: SyncFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for SyncFlags {
    type Output = SyncFlags;

    fn bitor(self, rhs: SyncFlags) -> SyncFlags {
        SyncFlags(self.0 | rhs.0)
    }
}

/// A reference-counted DRM sync-object timeline.
///
/// [`Clone`] takes another reference; [`Drop`] releases one and the kernel
/// object goes away with the last. See this module's own doc for why the DRM
/// descriptor it was created on is a documented requirement rather than a
/// borrow.
pub struct SyncTimeline {
    raw: NonNull<sys::wlr_drm_syncobj_timeline>,
}

impl SyncTimeline {
    /// Create a fresh timeline on `drm_fd`.
    ///
    /// `drm_fd` must stay open for as long as this timeline and every clone of
    /// it lives: wlroots keeps the descriptor and uses it to destroy the kernel
    /// object.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if the `DRM_IOCTL_SYNCOBJ_CREATE` ioctl failed —
    /// which is what a descriptor that is not a DRM device answers.
    pub fn create(drm_fd: BorrowedFd<'_>) -> Result<SyncTimeline> {
        // SAFETY: the borrow keeps the descriptor open for the call; keeping it
        // open afterwards is this function's documented requirement, which no
        // borrow here could express (see the module doc).
        let raw = unsafe { sys::wlr_drm_syncobj_timeline_create(drm_fd.as_raw_fd()) };
        SyncTimeline::adopt(raw, "wlr_drm_syncobj_timeline_create")
    }

    /// Import a timeline another process exported, as
    /// [`export`](SyncTimeline::export) produces.
    ///
    /// `syncobj_fd` is borrowed — wlroots turns it into a handle and never
    /// stores it, so the caller still owns and closes it. `drm_fd` is retained,
    /// exactly as in [`create`](SyncTimeline::create).
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if the descriptor was not an exported sync object on
    /// this device.
    pub fn import(drm_fd: BorrowedFd<'_>, syncobj_fd: BorrowedFd<'_>) -> Result<SyncTimeline> {
        // SAFETY: both descriptors are open for the call; only `drm_fd` is
        // retained past it (`render/drm_syncobj.c` converts `syncobj_fd` to a
        // handle immediately and forgets it).
        let raw = unsafe {
            sys::wlr_drm_syncobj_timeline_import(drm_fd.as_raw_fd(), syncobj_fd.as_raw_fd())
        };
        SyncTimeline::adopt(raw, "wlr_drm_syncobj_timeline_import")
    }

    fn adopt(raw: *mut sys::wlr_drm_syncobj_timeline, what: &'static str) -> Result<SyncTimeline> {
        Ok(SyncTimeline {
            raw: NonNull::new(raw).ok_or(Error::Create(what))?,
        })
    }

    /// The DRM device this timeline lives on, borrowed.
    ///
    /// The descriptor is the one [`create`](SyncTimeline::create) was given and
    /// wlroots kept; closing it would break every later operation, destruction
    /// included, which is why it is never an `OwnedFd`.
    pub fn drm_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: this value holds a live reference, so the struct is live, and
        // `drm_fd` is a public field wlroots writes once at creation. The
        // descriptor's validity is the caller's own obligation from `create`.
        unsafe { BorrowedFd::borrow_raw((*self.raw.as_ptr()).drm_fd) }
    }

    /// Export this timeline as a descriptor another process can
    /// [`import`](SyncTimeline::import).
    ///
    /// # Errors
    ///
    /// The `io::Error` the `DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD` ioctl set `errno`
    /// to. wlroots reports only `-1`, so the error is read back from `errno`
    /// immediately after the call rather than invented.
    pub fn export(&self) -> std::io::Result<OwnedFd> {
        // SAFETY: this value holds a live reference to the timeline.
        let fd = unsafe { sys::wlr_drm_syncobj_timeline_export(self.raw.as_ptr()) };
        // SAFETY: a non-negative return is a fresh descriptor wlroots hands
        // over — "the caller must close it" — so taking ownership is correct
        // and happens exactly once.
        owned_or_errno(fd)
    }

    /// Export the fence at `point` as a sync-file descriptor.
    ///
    /// Wait-before-signal is not supported: the point must already have a fence
    /// materialised, or this fails.
    ///
    /// # Errors
    ///
    /// The `io::Error` the failing ioctl set `errno` to.
    pub fn export_sync_file(&self, point: u64) -> std::io::Result<OwnedFd> {
        // SAFETY: as above.
        let fd =
            unsafe { sys::wlr_drm_syncobj_timeline_export_sync_file(self.raw.as_ptr(), point) };
        // SAFETY: as above — a fresh, owned descriptor on success.
        owned_or_errno(fd)
    }

    /// Import a sync-file's fence into `point`.
    ///
    /// `fd` is borrowed: wlroots transfers the fence into a temporary sync
    /// object and destroys that, leaving the caller's descriptor untouched.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if any of the three ioctls involved failed.
    pub fn import_sync_file(&self, point: u64, fd: BorrowedFd<'_>) -> Result<()> {
        // SAFETY: this value holds a live reference, and the borrow keeps `fd`
        // open for the call, which is the only time wlroots reads it.
        let ok = unsafe {
            sys::wlr_drm_syncobj_timeline_import_sync_file(self.raw.as_ptr(), point, fd.as_raw_fd())
        };
        if ok {
            Ok(())
        } else {
            Err(Error::Operation(
                "wlr_drm_syncobj_timeline_import_sync_file",
            ))
        }
    }

    /// Copy the fence at `src`'s `src_point` into this timeline's `dst_point`.
    ///
    /// # Errors
    ///
    /// [`Error::Mismatch`] if the two timelines are on different DRM
    /// descriptors — `wlr_drm_syncobj_timeline_transfer` **asserts** they
    /// match, and the distro build of wlroots aborts the process on a failed
    /// assertion rather than returning. [`Error::Operation`] if the transfer
    /// ioctl failed.
    pub fn transfer(&self, dst_point: u64, src: &SyncTimeline, src_point: u64) -> Result<()> {
        if self.drm_fd().as_raw_fd() != src.drm_fd().as_raw_fd() {
            return Err(Error::Mismatch("SyncTimeline::transfer"));
        }
        // SAFETY: both values hold live references, and the check above
        // satisfies the assertion wlroots makes about their descriptors.
        let ok = unsafe {
            sys::wlr_drm_syncobj_timeline_transfer(
                self.raw.as_ptr(),
                dst_point,
                src.raw.as_ptr(),
                src_point,
            )
        };
        if ok {
            Ok(())
        } else {
            Err(Error::Operation("wlr_drm_syncobj_timeline_transfer"))
        }
    }

    /// Whether `point` is signalled, without blocking.
    ///
    /// The two answers wlroots returns separately are collapsed the only way
    /// that keeps both: `Err` means the ioctl itself failed, `Ok(false)` means
    /// it succeeded and the point is not ready.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT` failed for a
    /// reason other than the timeout, which is what an unsignalled point with
    /// no [`SyncFlags::WAIT_FOR_SUBMIT`] looks like.
    pub fn check(&self, point: u64, flags: SyncFlags) -> Result<bool> {
        let mut result = false;
        // SAFETY: this value holds a live reference, and `result` is a live
        // local wlroots writes exactly once on success.
        let ok = unsafe {
            sys::wlr_drm_syncobj_timeline_check(
                self.raw.as_ptr(),
                point,
                flags.bits(),
                &raw mut result,
            )
        };
        if ok {
            Ok(result)
        } else {
            Err(Error::Operation("wlr_drm_syncobj_timeline_check"))
        }
    }

    /// Signal `point` from the CPU.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if the signal ioctl failed.
    pub fn signal(&self, point: u64) -> Result<()> {
        // SAFETY: this value holds a live reference to the timeline.
        let ok = unsafe { sys::wlr_drm_syncobj_timeline_signal(self.raw.as_ptr(), point) };
        if ok {
            Ok(())
        } else {
            Err(Error::Operation("wlr_drm_syncobj_timeline_signal"))
        }
    }

    /// The raw timeline, for interop with code that speaks `wlr-sys` directly.
    ///
    /// Borrowed: this value still holds the reference it will release on drop.
    pub fn as_ptr(&self) -> *mut sys::wlr_drm_syncobj_timeline {
        self.raw.as_ptr()
    }
}

/// Turn wlroots' `-1`-or-descriptor into an owned descriptor or the `errno`
/// the failing ioctl left behind.
///
/// wlroots logs and discards the real error, so `errno` is the only detail
/// available; it is read immediately, before anything else can overwrite it.
fn owned_or_errno(fd: std::os::raw::c_int) -> std::io::Result<OwnedFd> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a non-negative return from `wlr_drm_syncobj_timeline_export*` is
    // a fresh descriptor whose ownership wlroots hands to the caller, and this
    // is the only place it is claimed.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl Clone for SyncTimeline {
    /// Takes another reference to the same kernel object.
    fn clone(&self) -> SyncTimeline {
        // SAFETY: this value holds a live reference, so the count is at least
        // one and incrementing it cannot resurrect a freed object.
        let raw = unsafe { sys::wlr_drm_syncobj_timeline_ref(self.raw.as_ptr()) };
        SyncTimeline {
            raw: NonNull::new(raw).expect("wlr_drm_syncobj_timeline_ref returns its argument"),
        }
    }
}

impl std::fmt::Debug for SyncTimeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SAFETY: this value holds a live reference, so the struct is live and
        // both fields are public ones wlroots writes at creation.
        let (drm_fd, handle) =
            unsafe { ((*self.raw.as_ptr()).drm_fd, (*self.raw.as_ptr()).handle) };
        f.debug_struct("SyncTimeline")
            .field("drm_fd", &drm_fd)
            .field("handle", &handle)
            .finish()
    }
}

impl Drop for SyncTimeline {
    fn drop(&mut self) {
        // SAFETY: this value holds exactly one reference and releases it
        // exactly once here. The DRM descriptor wlroots uses to destroy the
        // kernel object at the last release is the caller's obligation to have
        // kept open — see this module's doc.
        unsafe { sys::wlr_drm_syncobj_timeline_unref(self.raw.as_ptr()) };
    }
}

/// The waiter as wlroots sees it, plus the state the trampoline needs.
///
/// `#[repr(C)]` with the C struct **first** so a `*mut wlr_drm_syncobj_timeline_waiter`
/// and a `*mut WaiterInner` are the same address: the callback is handed only
/// the former and has to recover the latter. That is `container_of` for a
/// zero-offset field, which is a plain cast.
#[repr(C)]
struct WaiterInner {
    /// Must stay the first field. See above.
    waiter: sys::wlr_drm_syncobj_timeline_waiter,

    /// Taken by the trampoline when the point becomes ready. `None` afterwards,
    /// so a callback that somehow re-entered could not run twice.
    callback: Cell<Option<Box<dyn FnOnce()>>>,
}

/// A one-shot wait on a timeline point, registered on an event loop.
///
/// Created by [`EventLoop::wait_for_timeline`]. The callback fires at most
/// once, from inside `wl_event_loop_dispatch`; dropping the waiter first
/// cancels it. Either way `Drop` releases the eventfd and the event source,
/// which is why the waiter borrows the loop: `wl_event_source_remove` on a
/// destroyed loop is a use-after-free.
///
/// The timeline does **not** have to outlive the waiter — wlroots reads its
/// handle and descriptor during registration and keeps neither
/// (`render/drm_syncobj.c`).
pub struct SyncWaiter<'l> {
    /// Boxed because wlroots stores this address in a libwayland event source,
    /// so it has to survive the `SyncWaiter` being moved.
    inner: Box<WaiterInner>,
    _loop: std::marker::PhantomData<&'l ()>,
}

impl SyncWaiter<'_> {
    /// Whether the callback has already run.
    ///
    /// Dropping the waiter is correct either way; this is for a compositor that
    /// wants to know whether the point arrived or the wait was abandoned.
    pub fn fired(&self) -> bool {
        let taken = self.inner.callback.take();
        let fired = taken.is_none();
        self.inner.callback.set(taken);
        fired
    }
}

impl std::fmt::Debug for SyncWaiter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncWaiter")
            .field("fired", &self.fired())
            .finish()
    }
}

impl Drop for SyncWaiter<'_> {
    fn drop(&mut self) {
        // `finish` is legal whether or not the callback has fired: the callback
        // path reads the eventfd and returns without touching the event source
        // or the descriptor (`handle_eventfd_ready`, `render/drm_syncobj.c`),
        // so those are still live and still ours to release. It is *not*
        // idempotent, which is why nothing else in this type calls it.
        //
        // SAFETY: `inner.waiter` was successfully initialised by
        // `wait_for_timeline` — the only constructor — and this is its one and
        // only `finish`. `'l` guarantees the event loop holding the source is
        // still alive.
        unsafe {
            sys::wlr_drm_syncobj_timeline_waiter_finish(&raw mut self.inner.waiter);
        }
    }
}

/// [`on_timeline_ready`] as the typedef wlroots declares the slot with.
///
/// Spelled out rather than passed inline so the function's signature is checked
/// against bindgen's `wlr_drm_syncobj_timeline_ready_callback` at compile time.
/// An `Option<extern "C" fn(..)>` parameter would coerce a *differently shaped*
/// function just as happily if the typedef ever changed.
const READY_CALLBACK: sys::wlr_drm_syncobj_timeline_ready_callback = Some(on_timeline_ready);

/// The `wlr_drm_syncobj_timeline_ready_callback` handed to wlroots.
///
/// # Safety
///
/// Called only by wlroots, with the `waiter` pointer `wait_for_timeline`
/// registered — which is the first field of a live [`WaiterInner`].
unsafe extern "C" fn on_timeline_ready(waiter: *mut sys::wlr_drm_syncobj_timeline_waiter) {
    let taken = {
        // SAFETY: `waiter` is the address of a live `WaiterInner`'s first
        // field, and `#[repr(C)]` makes the two addresses equal. The box
        // outlives this call: dropping the `SyncWaiter` would have removed the
        // event source this is being dispatched from.
        let inner = unsafe { &*waiter.cast::<WaiterInner>() };
        inner.callback.take()
    };
    // The borrow above ends here on purpose: the callback may reach code that
    // drops the `SyncWaiter` (through the compositor's own state), which takes
    // `&mut` of the very box that reference pointed into.
    let Some(callback) = taken else {
        return;
    };

    // libwayland is dispatching this loop right now, so this is a handler
    // frame: the loop must not be driven from inside it, and an unwind out of
    // an `extern "C"` frame aborts the process.
    let _frame = crate::dispatch::ForeignFrame::enter();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
}

impl<'l> EventLoop<'l> {
    /// Call `on_ready` from this loop once `point` on `timeline` is ready.
    ///
    /// "Ready" is what `flags` says: with [`SyncFlags::WAIT_AVAILABLE`] the
    /// callback fires as soon as a fence materialises, and with
    /// [`SyncFlags::NONE`] when the point actually signals. Those are the only
    /// two the kernel accepts here — `DRM_IOCTL_SYNCOBJ_EVENTFD` rejects every
    /// other bit with `EINVAL`, which surfaces as [`Error::Create`].
    ///
    /// The callback runs inside a dispatch of this loop, so the usual handler
    /// rules apply: it must not drive the loop again
    /// ([`EventLoop::dispatch`] refuses while it runs) and it must not panic —
    /// an unwind is caught and swallowed here rather than allowed to cross the
    /// `extern "C"` frame and abort.
    ///
    /// Dropping the returned [`SyncWaiter`] cancels a wait that has not fired
    /// and releases the kernel eventfd either way.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if the eventfd could not be created, the
    /// `DRM_IOCTL_SYNCOBJ_EVENTFD` ioctl was refused, or the source could not
    /// be added to this loop.
    pub fn wait_for_timeline(
        &self,
        timeline: &SyncTimeline,
        point: u64,
        flags: SyncFlags,
        on_ready: impl FnOnce() + 'static,
    ) -> Result<SyncWaiter<'l>> {
        let mut inner = Box::new(WaiterInner {
            // `waiter_init` assigns this struct wholesale on success, so the
            // value here is only ever read on the failure path — where it is
            // dropped without `finish`, because nothing was registered.
            //
            // SAFETY: every field of `wlr_drm_syncobj_timeline_waiter` is valid
            // all-zero: an `int`, a raw pointer, and an
            // `Option<extern "C" fn>` whose null representation is `None`.
            // Zeroing also avoids naming wlroots' `WLR_PRIVATE` members, which
            // are private on purpose.
            waiter: unsafe { std::mem::zeroed() },
            callback: Cell::new(Some(Box::new(on_ready))),
        });

        // SAFETY: `&raw mut inner.waiter` is the box's stable address, which
        // wlroots stores in the event source it adds to this loop and hands
        // back to `on_timeline_ready`; the box outlives the source because
        // `SyncWaiter::drop` removes the source before freeing it. The timeline
        // is live for the call, which is the only time wlroots reads it, and
        // the loop is live for `'l`.
        let ok = unsafe {
            sys::wlr_drm_syncobj_timeline_waiter_init(
                &raw mut inner.waiter,
                timeline.as_ptr(),
                point,
                flags.bits(),
                self.as_ptr(),
                READY_CALLBACK,
            )
        };
        if !ok {
            // Nothing was registered, so `finish` must not run — it would
            // remove a null event source and close descriptor -1. Dropping the
            // box is the whole cleanup.
            return Err(Error::Create("wlr_drm_syncobj_timeline_waiter_init"));
        }

        Ok(SyncWaiter {
            inner,
            _loop: std::marker::PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Timelines and waiters reach wlroots state that is not thread-safe: the
    /// reference count is a bare `size_t`, and a waiter's event source belongs
    /// to one loop on one thread.
    #[test]
    fn nothing_in_this_module_crosses_a_thread() {
        use static_assertions::assert_not_impl_any;

        assert_not_impl_any!(SyncTimeline: Send, Sync);
        assert_not_impl_any!(SyncWaiter<'static>: Send, Sync);
    }

    /// `SyncFlags` is a set, so `contains` has to be a subset test rather than
    /// an overlap test — the same distinction `BufferCaps` makes.
    #[test]
    fn sync_flags_contains_is_a_subset_test() {
        let both = SyncFlags::WAIT_FOR_SUBMIT | SyncFlags::WAIT_AVAILABLE;
        assert!(both.contains(SyncFlags::WAIT_FOR_SUBMIT));
        assert!(both.contains(both));
        assert!(both.contains(SyncFlags::NONE));
        assert!(!SyncFlags::WAIT_FOR_SUBMIT.contains(SyncFlags::WAIT_AVAILABLE));
        assert_eq!(SyncFlags::default(), SyncFlags::NONE);
    }
}
