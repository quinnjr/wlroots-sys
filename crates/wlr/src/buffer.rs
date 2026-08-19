//! RGBA8888 pixel-buffer scene nodes, addressed by id.
//!
//! Every other node type this crate exposes (`wlr_scene_rect`, a toplevel's
//! own `wlr_scene_tree`) is created and owned entirely by wlroots — this
//! crate never touches its memory. A pixel buffer is different: the pixels
//! themselves are a plain `Vec<u8>` this crate allocates, and wlroots frees
//! it, because [`PixelBuffer`] implements `wlr_buffer_impl`, the one C
//! vtable this crate hands to C rather than receives from it. That is what
//! this module exists to do safely — everything else (the id, the table,
//! the by-id mutators) mirrors `scene.rs`'s [`RectId`](crate::RectId).
//!
//! # Refcount story
//!
//! [`create_pixel_buffer`] returns a fresh `wlr_buffer` with `n_locks == 0`
//! and `dropped == false` (what `wlr_buffer_init` leaves it at — see
//! `wlr_buffer.h`'s own doc on `wlr_buffer_drop`/`wlr_buffer_lock`: neither
//! is called by `wlr_buffer_init` itself). Every call site in `runtime.rs`
//! that hands a freshly created buffer to wlroots follows the same two-step
//! producer handoff:
//!
//! 1. `wlr_scene_buffer_create` (or `wlr_scene_buffer_set_buffer` for an
//!    update) takes its own consumer lock on the buffer internally
//!    (`n_locks` 0 → 1), and stores the pointer on the scene node.
//! 2. This crate immediately calls `wlr_buffer_drop`, releasing the
//!    producer's own reference (`dropped` false → true). Because `n_locks`
//!    is still 1 at that point, the buffer is *not* destroyed yet — only
//!    marked so that the next unlock, whenever it happens, finishes the
//!    job.
//!
//! From then on the scene node's lock, plus whatever locks any other
//! consumer takes — most notably a renderer's own per-buffer texture cache,
//! which locks the buffer again the first time it is textured (pixman and
//! GLES2 both do, independently of the scene) — are what keep the buffer
//! alive; destruction follows the *last* of them to unlock, not necessarily
//! the scene's own. `wlr_scene_node_destroy` and `wlr_scene_buffer_set_buffer`
//! (replacing an old buffer with a new one) each unlock the one buffer this
//! module's own handoff left the scene holding a lock on; if a renderer has
//! also locked it by then, that unlock merely drops `n_locks` by one rather
//! than to zero, and the buffer survives until the renderer's own unlock —
//! typically when its texture cache entry is itself destroyed — follows.
//! `dropped` is already `true` from this module's own handoff by the time
//! any of that happens, so whichever unlock is the one that finally brings
//! `n_locks` to 0 is the one that calls into [`PIXEL_BUFFER_IMPL`]'s
//! `destroy` and frees the `Box` [`create_pixel_buffer`] leaked — still
//! exactly once, just not necessarily from the call site this module itself
//! made. No call in this module ever calls `wlr_buffer_lock`/`wlr_buffer_unlock`
//! directly — only wlroots' own scene and renderer code does, each exactly
//! once per lock it takes.
//!
//! If `wlr_scene_buffer_create` itself fails (returns null), it never took
//! the consumer lock described above, so `n_locks` is still 0. The
//! `wlr_buffer_drop` call still runs unconditionally in that case (see
//! `Runtime::add_buffer`), and with `n_locks` already 0 that call *is* the
//! one that destroys the buffer immediately — the failure path frees the
//! same way the success path eventually does, just synchronously instead of
//! on a later unlock.
//!
//! `destroy` itself must call `wlr_buffer_finish` before freeing the
//! allocation — see [`pixel_destroy`]'s own doc for why, and why neither
//! `wlr_buffer_drop` nor `wlr_buffer_unlock` does it on our behalf.
//!
//! # `update_buffer` versus a renderer reading through `begin_data_ptr_access`
//!
//! wlroots itself guards a genuine overlap — `wlr_buffer_drop` and
//! `wlr_buffer_unlock` both assert `!accessing_data_ptr` before proceeding —
//! but that guard compiles out in an `NDEBUG` wlroots build, at which point
//! an overlap would be a real use-after-free rather than a caught abort. It
//! cannot happen here in practice: this crate's whole API runs on one
//! thread, and every `begin_data_ptr_access`/`end_data_ptr_access` bracket
//! wlroots opens lives entirely inside a renderer call that textures a
//! buffer (`pixman_texture_from_buffer`, `gles2_texture_from_buffer`) —
//! calls this crate never re-enters compositor code from, and so calls a
//! consumer's own [`Runtime::update_buffer`] can never land in the middle
//! of. The invariant this depends on is "the event loop is single-threaded
//! and a render pass does not call back into handler code" — true of this
//! crate today, and worth re-checking the day either stops being true.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::render::DmabufAttributesRef;
use crate::sys;

/// A `wlr_buffer` borrowed for the duration of a call.
///
/// Borrow-scoped like every other handle in this crate, and for the usual
/// reason: a buffer is reference-counted by wlroots and freed the moment its
/// last reference goes, which can be inside any wlroots call this crate makes.
/// The two *owning* forms live in the render module and both deref to this one:
/// [`OwnedBuffer`](crate::OwnedBuffer) (the producer reference an allocator
/// hands out, released with `wlr_buffer_drop`) and
/// [`LockedBuffer`](crate::LockedBuffer) (the consumer reference a swapchain
/// hands out, released with `wlr_buffer_unlock`). This module's own "Refcount
/// story" is what those two names mean; they are **not** interchangeable.
pub struct Buffer<'a> {
    raw: NonNull<sys::wlr_buffer>,
    _scope: PhantomData<&'a ()>,
}

impl<'a> Buffer<'a> {
    /// # Safety
    ///
    /// * `raw` must point at a live `wlr_buffer`.
    /// * The returned handle must not outlive the reference that keeps that
    ///   buffer alive — a lock, a producer reference, or the callback the
    ///   buffer was handed to.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_buffer) -> Buffer<'a> {
        Buffer {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_buffer"),
            _scope: PhantomData,
        }
    }

    /// The raw buffer, for the in-crate callers that pass it back to wlroots.
    pub(crate) fn as_ptr(&self) -> *mut sys::wlr_buffer {
        self.raw.as_ptr()
    }

    /// Width in pixels.
    pub fn width(&self) -> i32 {
        // SAFETY: the handle's lifetime guarantees the buffer is live.
        unsafe { (*self.raw.as_ptr()).width }
    }

    /// Height in pixels.
    pub fn height(&self) -> i32 {
        // SAFETY: the handle's lifetime guarantees the buffer is live.
        unsafe { (*self.raw.as_ptr()).height }
    }

    /// Whether every pixel of this buffer is fully opaque, which lets a
    /// compositor skip whatever is behind it.
    pub fn is_opaque(&self) -> bool {
        // SAFETY: as above; `wlr_buffer_is_opaque` only reads the buffer.
        unsafe { sys::wlr_buffer_is_opaque(self.raw.as_ptr()) }
    }

    /// This buffer's DMA-BUF attributes, if it has any.
    ///
    /// The descriptors in the returned view belong to the buffer and are valid
    /// for as long as it is — which is why this is a
    /// [`DmabufAttributesRef`](crate::DmabufAttributesRef) and not the owned
    /// form. Calling `wlr_dmabuf_attributes_finish` on these would close
    /// descriptors the buffer still uses and then close them a second time when
    /// the buffer dies.
    pub fn dmabuf(&self) -> Option<DmabufAttributesRef<'_>> {
        let mut attrs = std::mem::MaybeUninit::<sys::wlr_dmabuf_attributes>::zeroed();
        // SAFETY: the handle's lifetime guarantees the buffer is live;
        // `attrs` is a live local wlroots fills in completely when it returns
        // true, and leaves alone otherwise (which is why the `false` arm
        // returns before assuming anything about it).
        let ok = unsafe { sys::wlr_buffer_get_dmabuf(self.raw.as_ptr(), attrs.as_mut_ptr()) };
        if !ok {
            return None;
        }
        // SAFETY: wlroots returned true, so every field is initialised. The
        // descriptors it names are the buffer's own, borrowed for `'_`.
        unsafe { DmabufAttributesRef::from_raw(attrs.assume_init()) }
    }
}

impl std::fmt::Debug for Buffer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}

/// Identifies an RGBA pixel-buffer scene node.
///
/// Not addon-backed, for the identical reason [`RectId`](crate::RectId)
/// isn't: nothing announces a `wlr_scene_buffer`'s destruction to this
/// crate on its own (it dies with its tree, or with an explicit
/// [`Runtime::remove_buffer`](crate::Runtime::remove_buffer)), so there is
/// no destroy hook to key an addon off. Drawn from the same monotonic
/// counter every other id in this crate uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(pub(crate) u64);

impl BufferId {
    /// An id no live buffer can have, for testing that an unknown id misses
    /// cleanly rather than crashing.
    ///
    /// The same reserved top of the counter's range
    /// [`ToplevelId::dangling_for_test`](crate::ToplevelId::dangling_for_test)
    /// uses, and for the same reason: ids come from a process-wide counter
    /// that starts at 1 and never reuses a value, so `u64::MAX` cannot be
    /// handed to a real buffer.
    pub fn dangling_for_test() -> BufferId {
        BufferId(u64::MAX)
    }
}

/// `DRM_FORMAT_ABGR8888`: bytes R, G, B, A in memory order (fourcc
/// `'A','B','2','4'`, little-endian channel order per the DRM fourcc
/// convention — the name is read MSB-first, the bytes are stored LSB-first).
/// This is the one format [`pixel_begin_data_ptr_access`] ever reports, and
/// it is what [`Runtime::add_buffer`](crate::Runtime::add_buffer)'s and
/// [`Runtime::update_buffer`](crate::Runtime::update_buffer)'s docs promise
/// callers: pixels are R, G, B, A per pixel, row-major.
const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;

/// A `wlr_buffer` backed by owned RGBA8888 pixels, plus the stride wlroots
/// needs to interpret them.
///
/// `#[repr(C)]` with `base` as field 0 is load-bearing: every callback below
/// receives a `*mut wlr_buffer`, and recovers this whole struct (and so the
/// `Box` [`create_pixel_buffer`] leaked) by casting that pointer straight to
/// `*mut PixelBuffer` — sound only because the two share their first byte.
#[repr(C)]
struct PixelBuffer {
    /// Must stay field 0 — see this struct's own doc.
    base: sys::wlr_buffer,
    stride: i32,
    data: Vec<u8>,
}

/// wlroots' one-and-only call to free this buffer, made when the last lock
/// on it is released after it has been dropped (see this module's own
/// "Refcount story" doc).
///
/// A `wlr_buffer_impl::destroy` implementation is required to call
/// `wlr_buffer_finish` on `buffer` before freeing its allocation — it is
/// what emits `buffer->events.destroy` and tears down `buffer->addons`
/// (`wlr_addon_set_finish`), and neither `wlr_buffer_drop` nor
/// `wlr_buffer_unlock` does it for the implementation; both tail-call
/// straight into `destroy` once the refcount reaches zero. Skipping it would
/// leave any destroy listener a consumer registered never told, and any
/// addon a consumer attached to `buffer->addons` left with `wl_list` links
/// pointing into the allocation this function is about to free — a
/// use-after-free at that consumer's own teardown, arbitrarily far from
/// here. wlroots' own `readonly_data_buffer_destroy` (the built-in impl
/// closest to this one) is `wlr_buffer_finish` immediately followed by
/// freeing the buffer, which is the order mirrored below.
unsafe extern "C" fn pixel_destroy(buffer: *mut sys::wlr_buffer) {
    // SAFETY: `buffer` is still a fully live `wlr_buffer` at this point —
    // wlroots has decided to destroy it but has not freed anything yet —
    // so finishing it here (emitting `events.destroy`, tearing down
    // `addons`) is exactly what its contract requires before anything is
    // freed. Must run *before* the `Box::from_raw` below: after that call
    // this pointer is dangling, and `wlr_buffer_finish` both reads and
    // writes through it.
    unsafe { sys::wlr_buffer_finish(buffer) };
    // SAFETY: `buffer` is the first field of the `PixelBuffer` `create_pixel_buffer`
    // leaked via `Box::into_raw`, so the cast recovers that exact allocation.
    // wlroots calls a buffer's `destroy` exactly once, only once every lock
    // on it (this module never takes one directly; see the module doc) has
    // been released — so this runs exactly once, and nothing else can be
    // holding this pointer when it does. `wlr_buffer_finish` just above does
    // not invalidate the allocation itself (it only mutates fields within
    // it), so the cast is still sound here.
    drop(unsafe { Box::from_raw(buffer.cast::<PixelBuffer>()) });
}

/// Hands wlroots a pointer straight at the owned pixel `Vec`.
///
/// Despite the `begin`/`end` bracket's name, at least one real consumer
/// keeps the returned pointer well past `end_data_ptr_access`: the pixman
/// renderer's `pixman_texture_from_buffer` calls `begin`, immediately calls
/// `end`, and only *afterward* builds a `pixman_image` directly over the
/// pointer it was handed, retaining it for the image's — and so the
/// texture's — whole life. So this cannot rely on "nothing outlives the
/// bracket"; see this function's own `SAFETY` comment for what it relies on
/// instead.
///
/// `flags` (`WLR_BUFFER_DATA_PTR_ACCESS_READ`/`_WRITE`) is intentionally
/// ignored: every access mode reads (and can write) the same plain `Vec`
/// backing this buffer, since it is a CPU-side allocation with no separate
/// read-only mapping to enforce a distinction against. Unlike a real
/// `wlr_buffer_impl` backed by GPU or shared memory, there is no cheaper or
/// safer thing to hand back for `READ` than for `WRITE`.
unsafe extern "C" fn pixel_begin_data_ptr_access(
    buffer: *mut sys::wlr_buffer,
    _flags: u32,
    data: *mut *mut std::ffi::c_void,
    format: *mut u32,
    stride: *mut usize,
) -> bool {
    // SAFETY: `buffer` is the first field of the `PixelBuffer` `create_pixel_buffer`
    // leaked via `Box::into_raw`, so the cast recovers that exact
    // allocation, and it is live for as long as any consumer can reach this
    // buffer at all (this crate never frees it itself; only `pixel_destroy`,
    // run by wlroots, does — see the module doc). The pointer handed out
    // through `*data` is sound to retain **past** this call, unlike this
    // function's own doc first suggests a `begin`/`end` bracket should be:
    // it is sound because `pb.data` is never mutated in place and never
    // reallocated for the whole life of this `PixelBuffer` — the `Vec` is
    // written once, in `create_pixel_buffer`, and from then on only ever
    // replaced wholesale (`Runtime::update_buffer` builds an entirely new
    // `PixelBuffer` and hands wlroots a new `wlr_buffer` rather than editing
    // this one's `Vec`). A future change that mutated `pb.data` in place
    // when a size matched would be an immediate use-after-free/torn-read
    // against any renderer (pixman, confirmed above) that has retained this
    // exact pointer past its own `end_data_ptr_access` — do not add one. The
    // three out-parameters are wlroots' own live stack locals for the call.
    let pb = unsafe { &mut *buffer.cast::<PixelBuffer>() };
    unsafe {
        *data = pb.data.as_mut_ptr().cast();
        *format = DRM_FORMAT_ABGR8888;
        *stride = pb.stride as usize;
    }
    true
}

/// The other half of the `begin`/`end` bracket. Nothing to release: this
/// buffer keeps its own storage for its whole life, so there is no lock or
/// mapping to tear down here — only `begin_data_ptr_access` had anything to
/// hand out.
unsafe extern "C" fn pixel_end_data_ptr_access(_buffer: *mut sys::wlr_buffer) {}

/// This crate's one `wlr_buffer_impl`. `get_dmabuf`/`get_shm` are `None`: a
/// pixel buffer is neither, and leaving them `None` is what tells wlroots
/// so — see `sys::wlr_buffer_impl`'s doc on the capability bits each
/// implemented function advertises.
static PIXEL_BUFFER_IMPL: sys::wlr_buffer_impl = sys::wlr_buffer_impl {
    destroy: Some(pixel_destroy),
    get_dmabuf: None,
    get_shm: None,
    begin_data_ptr_access: Some(pixel_begin_data_ptr_access),
    end_data_ptr_access: Some(pixel_end_data_ptr_access),
};

/// Validates that `width`/`height` are positive and `rgba` is exactly the
/// RGBA8888 byte length they imply, without overflowing along the way.
///
/// The one copy of a predicate `runtime.rs`'s `add_buffer`,
/// `add_buffer_in_toplevel` and `update_buffer` all need identically —
/// previously triplicated, which meant every future fix (this overflow
/// guard included) had to be made three times in lockstep. All arithmetic
/// runs in `u64` specifically so it cannot overflow on a 32-bit `usize`
/// target before the final comparison against `rgba_len` (the product of
/// two `i32::MAX`-bounded values times 4 is well under `u64::MAX`; see this
/// crate's own review notes for the exact bound). `width` is additionally
/// capped at `i32::MAX / 4`, which is what keeps `create_pixel_buffer`'s own
/// `stride = width * 4` from overflowing `i32` — anything this predicate
/// accepts is guaranteed safe for that multiply too.
pub(crate) fn validate_pixels(width: i32, height: i32, rgba_len: usize) -> bool {
    if width < 1 || height < 1 || width > i32::MAX / 4 {
        return false;
    }
    // Both casts are lossless: `width`/`height` are already known `>= 1`
    // here, so as `u32` they equal their `i32` value exactly.
    let expected = u64::from(width as u32) * u64::from(height as u32) * 4;
    expected == rgba_len as u64
}

/// Allocate a `wlr_buffer` backed by a copy of `rgba`, leaked to the heap so
/// wlroots can own it from here on.
///
/// `width`, `height` and `rgba`'s length must already have passed
/// [`validate_pixels`] — this takes that on faith and copies whatever it is
/// given. In particular `stride = width * 4` below relies on
/// [`validate_pixels`]'s `width <= i32::MAX / 4` bound to not overflow
/// `i32`; the `debug_assert!` restates that preconditon so a future caller
/// that skips validation fails loudly in a debug build rather than silently
/// wrapping into a negative stride in release.
///
/// The returned pointer has `n_locks == 0` and `dropped == false` (what
/// `wlr_buffer_init` leaves a fresh buffer at); see this module's own
/// "Refcount story" doc for what every caller in `runtime.rs` does with it
/// next.
pub(crate) fn create_pixel_buffer(width: i32, height: i32, rgba: &[u8]) -> *mut sys::wlr_buffer {
    debug_assert!(
        width > 0 && height > 0 && width <= i32::MAX / 4,
        "create_pixel_buffer called with unvalidated dimensions"
    );
    let mut pb = Box::new(PixelBuffer {
        // SAFETY: `wlr_buffer_init` below overwrites every field
        // `wlr_buffer`'s own contract requires a caller to set
        // (`impl_`/`width`/`height`, and it initialises `n_locks`,
        // `dropped`, `events` and `addons` itself) before this value is
        // read by anything else. Zeroed is a valid bit pattern to hold in
        // the meantime — it is never dereferenced before `wlr_buffer_init`
        // runs, on the very next line.
        base: unsafe { std::mem::zeroed() },
        stride: width * 4,
        data: rgba.to_vec(),
    });
    // SAFETY: `&mut pb.base` is a live, exclusively-owned `wlr_buffer` at
    // field offset 0 of `pb` (see `PixelBuffer`'s own doc on why that
    // offset matters); `&PIXEL_BUFFER_IMPL` is `'static` and every
    // `Option` field on it either points at a valid `extern "C" fn` or is
    // `None`, which `wlr_buffer_init` treats as an unsupported capability
    // rather than dereferencing it.
    unsafe { sys::wlr_buffer_init(&mut pb.base, &PIXEL_BUFFER_IMPL, width, height) };
    // Leaked deliberately: `pixel_destroy` is the only thing that ever
    // reclaims this `Box`, and it does so via `Box::from_raw` on the exact
    // pointer `into_raw` returns here.
    Box::into_raw(pb).cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest `width` [`validate_pixels`] accepts (with a matching
    /// `height`/length) is exactly `i32::MAX / 4` — one more, and
    /// `create_pixel_buffer`'s `stride = width * 4` would overflow `i32`.
    #[test]
    fn validate_pixels_accepts_the_widest_width_that_keeps_stride_in_i32_bounds() {
        let width = i32::MAX / 4;
        let len = width as u64 * 4;
        assert!(validate_pixels(width, 1, len as usize));
    }

    /// One past that bound must be rejected outright, regardless of whether
    /// `rgba_len` would otherwise "match" — this is the overflow guard, not
    /// the length check.
    #[test]
    fn validate_pixels_rejects_the_width_one_past_the_stride_overflow_bound() {
        let width = i32::MAX / 4 + 1;
        // Deliberately not overflow-checked here: this is a plain test
        // computation, not the guarded path under test.
        let len = (width as u64) * 4;
        assert!(!validate_pixels(width, 1, len as usize));
    }

    /// `height < 1` is rejected independently of `width` or `rgba_len`.
    #[test]
    fn validate_pixels_rejects_zero_height() {
        assert!(!validate_pixels(1, 0, 4));
    }

    /// A `width`/`height` that are individually fine must still be rejected
    /// if `rgba_len` doesn't match `width * height * 4` exactly.
    #[test]
    fn validate_pixels_rejects_a_mismatched_length() {
        assert!(!validate_pixels(2, 2, 2 * 2 * 4 - 1));
        assert!(!validate_pixels(2, 2, 2 * 2 * 4 + 1));
    }

    /// Pins the premise `dangling_for_test` rests on: the value it hands back
    /// sits above anything the real counter can reach, so "every by-id call
    /// misses on it" stays true rather than becoming true by luck.
    #[test]
    fn dangling_for_test_is_far_outside_the_real_id_space() {
        assert!(BufferId::dangling_for_test().0 > u32::MAX as u64);
    }
}
