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
//! an overlap would be a real use-after-free rather than a caught abort. Two
//! separate things keep it from happening here.
//!
//! Every `begin_data_ptr_access`/`end_data_ptr_access` bracket wlroots opens
//! *for itself* lives entirely inside a renderer call that textures a buffer
//! (`pixman_texture_from_buffer`, `gles2_texture_from_buffer`) — calls this
//! crate never re-enters compositor code from, and so calls a consumer's own
//! [`Runtime::update_buffer`] can never land in the middle of, this crate's
//! whole API running on one thread.
//!
//! A bracket a *consumer* opens, through [`Buffer::begin_data_ptr_access`],
//! is not so contained: the guard it hands back is an ordinary value that can
//! be held across arbitrary other calls, including ones that make wlroots
//! open its own bracket on the same buffer. Every entry point in this crate
//! that can reach one therefore refuses while such a guard is live — see
//! [`Buffer::begin_data_ptr_access`]'s own doc for the list and for how each
//! is detected.
//!
//! The invariant the first half depends on is "the event loop is
//! single-threaded and a render pass does not call back into handler code" —
//! true of this crate today, and worth re-checking the day either stops being
//! true.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::render::{DmabufAttributesRef, FourCc, ShmAttributesRef};
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

    /// Whether a `begin_data_ptr_access`/`end_data_ptr_access` bracket is open
    /// on this buffer right now.
    ///
    /// The check every safe call that can make wlroots open its own bracket on
    /// a *named* buffer makes first — see [`Buffer::begin_data_ptr_access`]'s
    /// doc for why calling in regardless would abort the process.
    pub(crate) fn data_ptr_access_open(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the buffer is live; this is
        // a plain field read of a `bool` wlroots itself only ever sets from
        // its own paired begin/end calls.
        unsafe { (*self.raw.as_ptr()).accessing_data_ptr }
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

    /// This buffer's shared-memory attributes, if it has any.
    ///
    /// Exact sibling of [`Buffer::dmabuf`], and simpler: the one descriptor
    /// belongs to the buffer for its whole life, so there is no fd ownership
    /// to reason about beyond what [`ShmAttributesRef`] already borrows.
    pub fn shm(&self) -> Option<ShmAttributesRef<'_>> {
        let mut attrs = std::mem::MaybeUninit::<sys::wlr_shm_attributes>::zeroed();
        // SAFETY: the handle's lifetime guarantees the buffer is live; `attrs`
        // is a live local wlroots fills in completely when it returns true,
        // and leaves alone otherwise (which is why the `false` arm returns
        // before assuming anything about it).
        let ok = unsafe { sys::wlr_buffer_get_shm(self.raw.as_ptr(), attrs.as_mut_ptr()) };
        if !ok {
            return None;
        }
        // SAFETY: wlroots returned true, so every field is initialised. The
        // descriptor it names is the buffer's own, borrowed for `'_`.
        Some(unsafe { ShmAttributesRef::from_raw(attrs.assume_init()) })
    }

    /// Map this buffer's underlying storage for `flags`-permitted access
    /// (read, write, or both), returning a guard that releases the mapping
    /// when dropped.
    ///
    /// # The `accessing_data_ptr` assertion this stands in front of
    ///
    /// `types/buffer.c`'s `wlr_buffer_begin_data_ptr_access` asserts
    /// `!buffer->accessing_data_ptr` on entry, and `wlr_buffer_drop`/
    /// `wlr_buffer_unlock` assert the same flag is clear before they run — in
    /// a non-`NDEBUG` build (this crate's, on Arch) any of those reached while
    /// already accessing the pointer aborts the process. Three things are what
    /// keep all three out of reach, and the second is the one that is easy to
    /// miss:
    ///
    /// * This method reads `accessing_data_ptr` itself first and returns
    ///   `None` rather than calling in, so calling it twice cannot reach the
    ///   entry assert.
    /// * A renderer call is the *other* way into that same entry assert:
    ///   wlroots opens the bracket for itself inside
    ///   `pixman_texture_from_buffer`/`gles2_texture_from_buffer` and friends,
    ///   which is a plain safe call taking a shared `&Buffer` and so is not
    ///   blocked by any borrow the guard holds. Each such entry point checks
    ///   for an open mapping and refuses instead:
    ///   [`Renderer::texture_from_buffer`](crate::Renderer::texture_from_buffer),
    ///   [`Renderer::begin_buffer_pass`](crate::Renderer::begin_buffer_pass),
    ///   [`Texture::update_from_buffer`](crate::Texture::update_from_buffer)
    ///   and [`Pixman::buffer_image`](crate::Pixman::buffer_image) check the
    ///   one buffer they are handed;
    ///   [`Runtime::commit_output`](crate::Runtime::commit_output) and
    ///   [`Runtime::commit_scene_output`](crate::Runtime::commit_scene_output)
    ///   render whatever buffers the scene graph holds — which this crate
    ///   cannot enumerate — and so refuse while *any* mapping opened by this
    ///   method is live anywhere.
    /// * The borrow this method takes (`&self`, propagated into the returned
    ///   guard's own lifetime) makes it impossible to drop or unlock the
    ///   buffer while the guard is alive, which is what puts the two asserts
    ///   in `wlr_buffer_drop`/`wlr_buffer_unlock` out of reach.
    ///
    /// # Errors
    ///
    /// `None` if a mapping is already open on this buffer, if wlroots itself
    /// refused (for example, `flags` asked for an operation this buffer's
    /// implementation does not support), or if the mapping wlroots described
    /// is one whose extent this crate cannot bound — see
    /// [`BufferDataAccess::data`] for what that means and why the bound has to
    /// be computed rather than assumed.
    pub fn begin_data_ptr_access(&self, flags: DataPtrAccess) -> Option<BufferDataAccess<'_>> {
        if self.data_ptr_access_open() {
            return None;
        }
        let mut data = std::ptr::null_mut();
        let mut format = 0u32;
        let mut stride = 0usize;
        // SAFETY: the handle's lifetime guarantees the buffer is live; the
        // three out-parameters are live locals owned exclusively by this
        // call. `accessing_data_ptr` was just observed false, immediately
        // above, on this single-threaded crate (see `buffer.rs`'s own module
        // doc on why no other call can interleave), so this cannot re-enter
        // the assert the check above stands in front of.
        let ok = unsafe {
            sys::wlr_buffer_begin_data_ptr_access(
                self.raw.as_ptr(),
                flags.0,
                &raw mut data,
                &raw mut format,
                &raw mut stride,
            )
        };
        if !ok {
            return None;
        }
        let Some(data) = NonNull::new(data.cast::<u8>()) else {
            // A buffer implementation that reports success with a null
            // pointer would be a wlroots-side contract violation; refuse to
            // hand out a guard rather than build one over a null base. Still
            // has to close the bracket wlroots just opened.
            // SAFETY: `begin_data_ptr_access` just returned `true` for this
            // exact buffer, so `end` is the required, and sound, way to close
            // the bracket it opened.
            unsafe { sys::wlr_buffer_end_data_ptr_access(self.raw.as_ptr()) };
            return None;
        };
        let Some(len) = mapped_len(FourCc(format), self.width(), self.height(), stride) else {
            // Nothing this crate can prove is mapped, so there is no slice it
            // could hand out soundly; refuse the guard rather than build one
            // over an extent taken on faith.
            // SAFETY: as above — `begin` succeeded, so `end` must run.
            unsafe { sys::wlr_buffer_end_data_ptr_access(self.raw.as_ptr()) };
            return None;
        };
        OPEN_MAPPINGS.with(|open| open.set(open.get() + 1));
        Some(BufferDataAccess {
            raw: self.raw,
            data,
            len,
            format: FourCc(format),
            stride,
            flags,
            _scope: PhantomData,
        })
    }
}

thread_local! {
    /// How many [`BufferDataAccess`] guards are live on this thread.
    ///
    /// A count rather than a set of buffer pointers because the callers that
    /// consult it — the scene-output commits — render whatever buffers the
    /// scene graph happens to hold, which this crate does not enumerate, so
    /// the only question it can usefully answer is "is any mapping open at
    /// all". Thread-local rather than global because a `wlr_buffer` is only
    /// ever reached from the thread its display runs on, and a count shared
    /// between unrelated displays would make one compositor's mapping refuse
    /// another's commit.
    static OPEN_MAPPINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether any [`BufferDataAccess`] guard is live on this thread.
///
/// For the callers that can make wlroots open a data-pointer bracket on a
/// buffer they were never handed — see [`Buffer::begin_data_ptr_access`].
pub(crate) fn any_data_ptr_access_open() -> bool {
    OPEN_MAPPINGS.with(std::cell::Cell::get) != 0
}

/// How many bytes of the region `wlr_buffer_begin_data_ptr_access` handed back
/// are provably part of the image the buffer describes, or `None` if that
/// cannot be established at all.
///
/// `wlr_buffer.h` promises only that the pointer "should be pointing to a
/// valid memory region for the operations specified in the flags" — it names
/// no size whatsoever, and `wlr_buffer_impl` is a public vtable, so a
/// consumer's own implementation is as much a source of these numbers as
/// wlroots' built-in ones are. The largest extent that follows from what the
/// call itself reported is therefore `height - 1` whole strides plus one row
/// of `width` pixels: a mapping that did not cover at least that could not
/// hold the image its own `format`/`stride` claim to describe. It is
/// specifically *not* `stride * height`, which additionally assumes the last
/// row is padded out to a full stride — nothing requires that, and charging
/// it would put the tail of the slice past the mapping on any implementation
/// that does not pad.
///
/// `None` when the row length cannot be computed (a `format` outside wlroots'
/// own pixel-format table, where guessing low is unsound and guessing high is
/// worse), when the reported `stride` is too small to hold one row (a broken
/// implementation, whose numbers prove nothing), or when the arithmetic
/// overflows `usize`.
fn mapped_len(format: FourCc, width: i32, height: i32, stride: usize) -> Option<usize> {
    if width < 0 || height < 0 {
        return None;
    }
    if height == 0 {
        return Some(0);
    }
    // Lossless: `width` is known non-negative here.
    let row = usize::try_from(crate::render::pixel_format::min_stride(
        format,
        u64::from(width as u32),
    )?)
    .ok()?;
    if row > stride {
        return None;
    }
    // Lossless: `height` is known `>= 1` here.
    stride.checked_mul(height as usize - 1)?.checked_add(row)
}

impl std::fmt::Debug for Buffer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}

/// `WLR_BUFFER_DATA_PTR_ACCESS_READ`/`_WRITE`, the argument to
/// [`Buffer::begin_data_ptr_access`].
///
/// A bitmask of `enum wlr_buffer_data_ptr_access_flag`. Hand-rolled rather
/// than a `bitflags` dependency, following
/// [`BufferCaps`](crate::render::BufferCaps): the two bits are the whole
/// domain and their values are pinned by this module's own tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DataPtrAccess(u32);

impl DataPtrAccess {
    /// The buffer contents can be read back.
    pub const READ: DataPtrAccess =
        DataPtrAccess(sys::wlr_buffer_data_ptr_access_flag::WLR_BUFFER_DATA_PTR_ACCESS_READ.0);
    /// The buffer contents can be written to.
    pub const WRITE: DataPtrAccess =
        DataPtrAccess(sys::wlr_buffer_data_ptr_access_flag::WLR_BUFFER_DATA_PTR_ACCESS_WRITE.0);

    /// Whether every bit of `other` is set here.
    pub fn contains(self, other: DataPtrAccess) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for DataPtrAccess {
    type Output = DataPtrAccess;

    fn bitor(self, rhs: DataPtrAccess) -> DataPtrAccess {
        DataPtrAccess(self.0 | rhs.0)
    }
}

/// A scoped mapping of a buffer's underlying storage, opened by
/// [`Buffer::begin_data_ptr_access`] and closed by `Drop` — the
/// `wlr_buffer_end_data_ptr_access` half of the pairing is not exposed as its
/// own function so that the header's "must be called after begin" rule is
/// discharged by the type system rather than by the caller.
///
/// Borrows the [`Buffer`] for its whole life: the header documents the
/// returned pointer as "only valid up to the next
/// `wlr_buffer_end_data_ptr_access()` call", and tying this guard to `'a`
/// additionally makes it impossible to drop or unlock the underlying buffer
/// (both of which assert `!accessing_data_ptr`, see
/// [`Buffer::begin_data_ptr_access`]'s own doc) while a mapping is open.
pub struct BufferDataAccess<'a> {
    raw: NonNull<sys::wlr_buffer>,
    data: NonNull<u8>,
    len: usize,
    format: FourCc,
    stride: usize,
    flags: DataPtrAccess,
    _scope: PhantomData<&'a ()>,
}

impl BufferDataAccess<'_> {
    /// The format the mapped bytes should be interpreted as.
    pub fn format(&self) -> FourCc {
        self.format
    }

    /// Byte stride of one row.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// The mapped bytes, if this mapping was opened with
    /// [`DataPtrAccess::READ`].
    ///
    /// The slice is `stride * (height - 1) + width` pixels' worth of bytes,
    /// which is every byte of the image the buffer describes and **not** one
    /// more. It is deliberately not `stride * height`: `wlr_buffer.h` documents
    /// the returned pointer's validity but names no size at all, and
    /// `wlr_buffer_impl` is a public vtable that a consumer of this crate can
    /// implement, so an implementation whose last row is not padded out to a
    /// full stride is legal and would put the tail of a `stride * height` slice
    /// past the end of the mapping. Padding after the last row, where an
    /// implementation does have it, is simply not exposed.
    ///
    /// A row is therefore addressed as `stride * y .. stride * y + row_bytes`
    /// rather than by chunking the slice into equal `stride`-sized pieces —
    /// the last chunk may be short.
    pub fn data(&self) -> Option<&[u8]> {
        if !self.flags.contains(DataPtrAccess::READ) {
            return None;
        }
        // SAFETY: `data` was established by a successful
        // `wlr_buffer_begin_data_ptr_access` in `Buffer::begin_data_ptr_access`
        // and is valid until the matching `end` this guard's `Drop` performs,
        // which cannot run before this borrow of `self` ends. `len` is the
        // extent that call's own reported `format`/`stride` and the buffer's
        // own `width`/`height` prove is mapped — see `mapped_len`, which
        // refuses to build this guard at all when they prove nothing.
        Some(unsafe { std::slice::from_raw_parts(self.data.as_ptr(), self.len) })
    }

    /// The mapped bytes, mutably, if this mapping was opened with
    /// [`DataPtrAccess::WRITE`].
    ///
    /// See [`BufferDataAccess::data`] for the extent this slice covers and
    /// how to address a row within it.
    pub fn data_mut(&mut self) -> Option<&mut [u8]> {
        if !self.flags.contains(DataPtrAccess::WRITE) {
            return None;
        }
        // SAFETY: as `data` above; `&mut self` here proves no other borrow of
        // this same slice is live for the duration of the returned one.
        Some(unsafe { std::slice::from_raw_parts_mut(self.data.as_ptr(), self.len) })
    }
}

impl Drop for BufferDataAccess<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` was a live `wlr_buffer` with a mapping this same
        // value's construction opened via a successful `begin_data_ptr_access`,
        // and this is the one place that ever closes it — the header requires
        // exactly this call after `begin`, and requires it exactly once.
        unsafe { sys::wlr_buffer_end_data_ptr_access(self.raw.as_ptr()) };
        // Paired with the increment `begin_data_ptr_access` makes on the one
        // path that constructs this type, so the count is back to zero once
        // the last guard is gone and the commits stop refusing.
        OPEN_MAPPINGS.with(|open| open.set(open.get().saturating_sub(1)));
    }
}

impl std::fmt::Debug for BufferDataAccess<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferDataAccess")
            .field("format", &self.format)
            .field("stride", &self.stride)
            .field("len", &self.len)
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

    /// The two bits are `1 << 0` and `1 << 1`, exactly as
    /// `enum wlr_buffer_data_ptr_access_flag` declares them in
    /// `wlr/types/wlr_buffer.h`. This is what makes hand-rolling the bitmask
    /// (rather than taking a `bitflags` dependency) a checked decision: the
    /// constants are re-exported from `wlr-sys`, so a wlroots renumbering
    /// would change them silently and only this test would notice.
    #[test]
    fn the_access_flags_are_the_two_bits_wlroots_declares() {
        assert_eq!(DataPtrAccess::READ.0, 1 << 0);
        assert_eq!(DataPtrAccess::WRITE.0, 1 << 1);
        assert_ne!(DataPtrAccess::READ, DataPtrAccess::WRITE);
    }

    /// `contains` is "every bit of the argument is set here", not "any" — the
    /// distinction that decides whether a read-only mapping hands out
    /// `data_mut`.
    #[test]
    fn contains_is_every_bit_not_any_bit() {
        let both = DataPtrAccess::READ | DataPtrAccess::WRITE;
        assert_eq!(both.0, 0b11);

        assert!(both.contains(DataPtrAccess::READ));
        assert!(both.contains(DataPtrAccess::WRITE));
        assert!(both.contains(both));

        assert!(DataPtrAccess::READ.contains(DataPtrAccess::READ));
        assert!(!DataPtrAccess::READ.contains(DataPtrAccess::WRITE));
        assert!(!DataPtrAccess::READ.contains(both));
        assert!(!DataPtrAccess::WRITE.contains(DataPtrAccess::READ));
    }

    /// The mapped extent stops at the end of the last row of pixels, not at
    /// the end of the last stride — a buffer whose rows are padded still only
    /// exposes `stride * (height - 1) + row` bytes.
    #[test]
    fn mapped_len_does_not_charge_padding_after_the_last_row() {
        // 4 bytes a pixel, 3 pixels a row, 16 bytes of stride: 8 padded bytes
        // after each row, but none charged after the last.
        assert_eq!(mapped_len(FourCc::ARGB8888, 3, 4, 16), Some(16 * 3 + 12));
        // Unpadded is the same answer `stride * height` would have given.
        assert_eq!(mapped_len(FourCc::ARGB8888, 3, 4, 12), Some(48));
        // One row, all padding: still just the row.
        assert_eq!(mapped_len(FourCc::ARGB8888, 3, 1, 4096), Some(12));
    }

    /// A format outside wlroots' own table gives no row length, and a stride
    /// too short to hold one row means the implementation's own numbers
    /// contradict each other. Neither can be turned into a slice.
    #[test]
    fn mapped_len_refuses_what_it_cannot_bound() {
        let unknown = FourCc::from_chars(b'?', b'?', b'?', b'?');
        assert_eq!(mapped_len(unknown, 3, 4, 16), None);
        assert_eq!(mapped_len(FourCc::ARGB8888, 3, 4, 11), None);
        assert_eq!(mapped_len(FourCc::ARGB8888, -1, 4, 16), None);
        assert_eq!(mapped_len(FourCc::ARGB8888, 3, -1, 16), None);
        assert_eq!(mapped_len(FourCc::ARGB8888, 3, i32::MAX, usize::MAX), None);
    }

    /// A zero-height buffer maps nothing at all; an empty slice is the honest
    /// answer, not a refusal.
    #[test]
    fn mapped_len_of_a_zero_height_buffer_is_empty() {
        assert_eq!(mapped_len(FourCc::ARGB8888, 3, 0, 16), Some(0));
    }

    /// Pins the premise `dangling_for_test` rests on: the value it hands back
    /// sits above anything the real counter can reach, so "every by-id call
    /// misses on it" stays true rather than becoming true by luck.
    #[test]
    fn dangling_for_test_is_far_outside_the_real_id_space() {
        assert!(BufferId::dangling_for_test().0 > u32::MAX as u64);
    }
}
