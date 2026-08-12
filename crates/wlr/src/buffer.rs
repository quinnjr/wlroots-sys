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
//! From then on the scene node's own lock is the only thing keeping the
//! buffer alive: `wlr_scene_node_destroy` and `wlr_scene_buffer_set_buffer`
//! (replacing an old buffer with a new one) both unlock the buffer they are
//! letting go of, which drops `n_locks` to 0. `dropped` is already `true` by
//! then, so that unlock is the one that calls into [`PIXEL_BUFFER_IMPL`]'s
//! `destroy` and frees the `Box` [`create_pixel_buffer`] leaked. No call in
//! this module ever calls `wlr_buffer_lock`/`wlr_buffer_unlock` directly —
//! only the two wlroots entry points above do, and always exactly once each,
//! for exactly this handoff.
//!
//! If `wlr_scene_buffer_create` itself fails (returns null), it never took
//! the consumer lock described above, so `n_locks` is still 0. The
//! `wlr_buffer_drop` call still runs unconditionally in that case (see
//! `Runtime::add_buffer`), and with `n_locks` already 0 that call *is* the
//! one that destroys the buffer immediately — the failure path frees the
//! same way the success path eventually does, just synchronously instead of
//! on a later unlock.

use crate::sys;

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
unsafe extern "C" fn pixel_destroy(buffer: *mut sys::wlr_buffer) {
    // SAFETY: `buffer` is the first field of the `PixelBuffer` `create_pixel_buffer`
    // leaked via `Box::into_raw`, so the cast recovers that exact allocation.
    // wlroots calls a buffer's `destroy` exactly once, only once every lock
    // on it (this module never takes one directly; see the module doc) has
    // been released — so this runs exactly once, and nothing else can be
    // holding this pointer when it does.
    drop(unsafe { Box::from_raw(buffer.cast::<PixelBuffer>()) });
}

/// Hands wlroots a pointer straight at the owned pixel `Vec`, for however
/// long it takes to render one frame from it.
unsafe extern "C" fn pixel_begin_data_ptr_access(
    buffer: *mut sys::wlr_buffer,
    _flags: u32,
    data: *mut *mut std::ffi::c_void,
    format: *mut u32,
    stride: *mut usize,
) -> bool {
    // SAFETY: same layout argument as `pixel_destroy`'s — `buffer` names a
    // live `PixelBuffer` for the duration of this call, because wlroots
    // brackets every `begin_data_ptr_access` with a matching
    // `end_data_ptr_access` and does not retain the pointer past it (see
    // `wlr_buffer.h`'s own doc on the pair). The three out-parameters are
    // wlroots' own live stack locals for the call.
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

/// Allocate a `wlr_buffer` backed by a copy of `rgba`, leaked to the heap so
/// wlroots can own it from here on.
///
/// `rgba` must already be validated by the caller (`width >= 1 && height >=
/// 1 && rgba.len() == width * height * 4`) — this takes it on faith and
/// copies whatever it is given.
///
/// The returned pointer has `n_locks == 0` and `dropped == false` (what
/// `wlr_buffer_init` leaves a fresh buffer at); see this module's own
/// "Refcount story" doc for what every caller in `runtime.rs` does with it
/// next.
pub(crate) fn create_pixel_buffer(width: i32, height: i32, rgba: &[u8]) -> *mut sys::wlr_buffer {
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
