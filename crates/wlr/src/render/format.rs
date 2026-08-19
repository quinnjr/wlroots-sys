//! DRM pixel formats, modifiers, and the sets wlroots reports them in.
//!
//! # Two shapes of format, and why both exist
//!
//! `wlr/render/drm_format_set.h` publishes `wlr_drm_format_finish` and nothing
//! that *builds* a `wlr_drm_format`: there is no `wlr_drm_format_create`, and no
//! public way to add a modifier to a single format. The only two ways to hold
//! one are to borrow it out of a set (`wlr_drm_format_set_get`, or
//! `wlr_swapchain.format`) or to fill the struct in by hand.
//!
//! Both are needed, because `wlr_swapchain_create` and
//! `wlr_allocator_create_buffer` take a `const struct wlr_drm_format *` a caller
//! has to supply. So this module has:
//!
//! * [`DrmFormat`] — Rust-owned, a fourcc plus a `Vec` of modifiers. It is
//!   materialised into a temporary `wlr_drm_format` for the duration of a call
//!   and is **never** handed to `wlr_drm_format_finish`, which would `free()` a
//!   `Vec`'s buffer.
//! * [`DrmFormatRef`] — a borrowed view of one wlroots owns.
//! * [`DrmFormatSet`] — an owned set, `Drop` → `wlr_drm_format_set_finish`.
//! * [`DrmFormatSetRef`] — a borrowed set, what
//!   [`Renderer::texture_formats`](crate::Renderer::texture_formats) returns.
//!
//! # The header comment on `formats` is wrong
//!
//! `wlr_drm_format_set.formats` is documented as "a pointer to an array of
//! `struct wlr_drm_format *`", but the declared type is `struct wlr_drm_format *`
//! — an array of structs *by value*. The type is right and the comment is wrong;
//! indexing it as an array of pointers produces plausible-looking garbage rather
//! than a crash. [`DrmFormatSetRef::iter`] indexes by value, and
//! `tests/render.rs` pins that against a set it built itself.

use std::marker::PhantomData;

use crate::error::{Error, Result};
use crate::sys;

/// A DRM `fourcc` pixel format code.
///
/// The wire values are the ones in libdrm's `drm_fourcc.h`; this crate spells
/// only the three a compositor reaches for constantly and leaves the rest to
/// [`FourCc::from_chars`], which builds any of them from the four characters
/// the DRM name is written with.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCc(pub u32);

impl FourCc {
    /// `DRM_FORMAT_ARGB8888`.
    pub const ARGB8888: FourCc = FourCc::from_chars(b'A', b'R', b'2', b'4');
    /// `DRM_FORMAT_XRGB8888`.
    pub const XRGB8888: FourCc = FourCc::from_chars(b'X', b'R', b'2', b'4');
    /// `DRM_FORMAT_ABGR8888` — the format
    /// [`Runtime::add_buffer`](crate::Runtime::add_buffer) hands wlroots.
    pub const ABGR8888: FourCc = FourCc::from_chars(b'A', b'B', b'2', b'4');

    /// `DRM_FORMAT_INVALID`, which is the zero code.
    ///
    /// wlroots uses it as "no format"; `wlr_drm_format_set_add` **asserts**
    /// against it, which is why [`DrmFormatSet::add`] rejects it before calling.
    pub const INVALID: FourCc = FourCc(0);

    /// Build a code from the four characters the DRM name is written with, in
    /// that order: `from_chars(b'A', b'R', b'2', b'4')` is `DRM_FORMAT_ARGB8888`.
    ///
    /// The characters are packed least-significant-byte first, matching
    /// libdrm's own `fourcc_code` macro.
    pub const fn from_chars(a: u8, b: u8, c: u8, d: u8) -> FourCc {
        FourCc((a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24))
    }
}

impl std::fmt::Debug for FourCc {
    /// Prints the four characters and the hex code, because a fourcc read as a
    /// bare number is unrecognisable and read as bare characters is ambiguous
    /// with the byte order.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0.to_le_bytes();
        write!(f, "FourCc(")?;
        for b in bytes {
            // Anything outside printable ASCII is spelled as an escape rather
            // than emitted raw: a format code this crate did not expect must
            // still print without corrupting the reader's terminal.
            if b.is_ascii_graphic() {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        write!(f, ", {:#010x})", self.0)
    }
}

/// A DRM format modifier: how the pixels of a format are actually laid out in
/// memory (tiling, compression, vendor-specific swizzles).
///
/// The contract from `wlr/render/dmabuf.h`, verbatim, because importers get it
/// wrong in ways that only show up on one vendor's hardware:
///
/// > If the buffer was allocated with explicit modifiers enabled, the `modifier`
/// > field must not be INVALID.
/// >
/// > If the buffer was allocated with explicit modifiers disabled (either
/// > because the driver doesn't support it, or because the user didn't specify a
/// > valid modifier list), the `modifier` field can have two values: INVALID
/// > means that an implicit vendor-defined modifier is in use, LINEAR means that
/// > the buffer is linear. The `modifier` field must not have any other value.
/// >
/// > When importing a DMA-BUF, users must not ignore the modifier unless it's
/// > INVALID or LINEAR. In particular, users must not import a DMA-BUF to a
/// > legacy API which doesn't support specifying an explicit modifier unless the
/// > modifier is set to INVALID or LINEAR.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Modifier(pub u64);

impl Modifier {
    /// `DRM_FORMAT_MOD_INVALID`: an implicit, vendor-defined layout.
    pub const INVALID: Modifier = Modifier(0x00ff_ffff_ffff_ffff);
    /// `DRM_FORMAT_MOD_LINEAR`: no tiling, no compression.
    pub const LINEAR: Modifier = Modifier(0);
}

/// A format and the modifiers it may be used with, owned by Rust.
///
/// Handed to wlroots by pointer for the duration of a single call. Never
/// `wlr_drm_format_finish`ed — that `free()`s the modifier array, which here
/// belongs to a `Vec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmFormat {
    /// The pixel format.
    pub format: FourCc,
    /// The acceptable layouts, in preference order. May be empty, which asks
    /// for whatever the allocator's default is.
    pub modifiers: Vec<Modifier>,
}

impl DrmFormat {
    /// A format with the given modifier list.
    pub fn new(format: FourCc, modifiers: impl IntoIterator<Item = Modifier>) -> DrmFormat {
        DrmFormat {
            format,
            modifiers: modifiers.into_iter().collect(),
        }
    }

    /// Run `f` with a `wlr_drm_format` that borrows this value's modifier
    /// storage.
    ///
    /// `capacity` is set equal to `len`, not left zero: `wlr_drm_format_copy`
    /// asserts `len <= capacity`, and that assertion is compiled into the
    /// shipped `libwlroots-0.20.so`. The `modifiers` pointer is cast to `*mut`
    /// because that is the field's declared type; every wlroots function this
    /// crate passes a hand-built format to (`wlr_swapchain_create`,
    /// `wlr_allocator_create_buffer`) takes it as `const` and only reads it.
    /// Nothing here may be handed to `wlr_drm_format_add`, which would `realloc`
    /// a pointer Rust owns.
    pub(crate) fn with_sys<R>(&self, f: impl FnOnce(*const sys::wlr_drm_format) -> R) -> R {
        // `Modifier` is a `#[repr(transparent)]`-equivalent newtype over `u64`
        // only by construction, so go through a plain `u64` view rather than
        // casting the slice: the cast would be an unchecked layout claim, and
        // this loop is bounded by a handful of modifiers.
        let modifiers: Vec<u64> = self.modifiers.iter().map(|m| m.0).collect();
        let raw = sys::wlr_drm_format {
            format: self.format.0,
            len: modifiers.len(),
            capacity: modifiers.len(),
            modifiers: modifiers.as_ptr().cast_mut(),
        };
        let out = f(&raw);
        // `modifiers` must outlive the call, and this is what says so to a
        // future reader (and to any optimiser that might otherwise see the
        // `Vec` as dead after `as_ptr`).
        drop(modifiers);
        out
    }
}

/// A borrowed view of a `wlr_drm_format` wlroots owns.
///
/// Invalidated by any mutation of the set it came from — wlroots reallocates
/// the array — which is why this borrows the set rather than copying out of it.
#[derive(Clone, Copy)]
pub struct DrmFormatRef<'a> {
    raw: *const sys::wlr_drm_format,
    _scope: PhantomData<&'a ()>,
}

impl<'a> DrmFormatRef<'a> {
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_drm_format` that outlives `'a` and is not
    /// mutated for that whole period.
    pub(crate) unsafe fn from_raw(raw: *const sys::wlr_drm_format) -> DrmFormatRef<'a> {
        DrmFormatRef {
            raw,
            _scope: PhantomData,
        }
    }

    /// The pixel format.
    pub fn format(&self) -> FourCc {
        // SAFETY: the borrow guarantees the format is live and unmutated.
        FourCc(unsafe { (*self.raw).format })
    }

    /// The layouts this format may be used with.
    pub fn modifiers(&self) -> impl Iterator<Item = Modifier> + '_ {
        // SAFETY: as above. wlroots keeps `len` modifiers contiguously at
        // `modifiers`; a zero-length format has a null pointer, which
        // `from_raw_parts` will not accept, so that case yields the empty slice
        // instead.
        let slice = unsafe {
            let raw = &*self.raw;
            if raw.len == 0 || raw.modifiers.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(raw.modifiers, raw.len)
            }
        };
        slice.iter().copied().map(Modifier)
    }

    /// Copy this format into one Rust owns.
    pub fn to_owned(&self) -> DrmFormat {
        DrmFormat {
            format: self.format(),
            modifiers: self.modifiers().collect(),
        }
    }
}

impl std::fmt::Debug for DrmFormatRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DrmFormatRef")
            .field("format", &self.format())
            .field("modifiers", &self.modifiers().collect::<Vec<_>>())
            .finish()
    }
}

/// The read-only half of a format set, shared by [`DrmFormatSet`] and
/// [`DrmFormatSetRef`] so the two cannot drift apart.
///
/// # Safety
///
/// Every function here requires `set` to point at a live, initialised
/// `wlr_drm_format_set`.
mod read {
    use super::{DrmFormatRef, FourCc, Modifier, sys};

    pub(super) unsafe fn len(set: *const sys::wlr_drm_format_set) -> usize {
        // SAFETY: the caller guarantees `set` is live; this only reads a field.
        unsafe { (*set).len }
    }

    pub(super) unsafe fn has(
        set: *const sys::wlr_drm_format_set,
        format: FourCc,
        modifier: Modifier,
    ) -> bool {
        // SAFETY: as above; `wlr_drm_format_set_has` only reads the set.
        unsafe { sys::wlr_drm_format_set_has(set, format.0, modifier.0) }
    }

    pub(super) unsafe fn get<'a>(
        set: *const sys::wlr_drm_format_set,
        format: FourCc,
    ) -> Option<DrmFormatRef<'a>> {
        // SAFETY: as above. The returned pointer is owned by the set and stays
        // valid until the set is mutated, which the caller's borrow forbids.
        let raw = unsafe { sys::wlr_drm_format_set_get(set, format.0) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: non-null, owned by a set the caller's borrow keeps alive.
        Some(unsafe { DrmFormatRef::from_raw(raw) })
    }

    pub(super) unsafe fn nth<'a>(
        set: *const sys::wlr_drm_format_set,
        index: usize,
    ) -> Option<DrmFormatRef<'a>> {
        // SAFETY: the caller guarantees `set` is live. `formats` is an array of
        // `wlr_drm_format` **by value** — see this module's own doc on the
        // header comment that says otherwise — so the element address is
        // `formats.add(index)`.
        unsafe {
            let raw = &*set;
            if index >= raw.len || raw.formats.is_null() {
                return None;
            }
            Some(DrmFormatRef::from_raw(raw.formats.add(index)))
        }
    }
}

/// An owned set of formats and their modifiers.
///
/// `Drop` calls `wlr_drm_format_set_finish`, which frees every format in the set
/// and the array holding them.
pub struct DrmFormatSet {
    /// Boxed so the address is stable: `intersect`/`union` write through a
    /// pointer to this, and a moved `DrmFormatSet` must not disturb it.
    raw: Box<sys::wlr_drm_format_set>,
}

impl DrmFormatSet {
    /// An empty set.
    pub fn new() -> DrmFormatSet {
        DrmFormatSet {
            // The zeroed value is what wlroots' own callers start from: `len`,
            // `capacity` and `formats` all read as "nothing allocated yet", and
            // every mutator grows from there.
            raw: Box::new(sys::wlr_drm_format_set {
                len: 0,
                capacity: 0,
                formats: std::ptr::null_mut(),
            }),
        }
    }

    fn as_ptr(&self) -> *const sys::wlr_drm_format_set {
        &raw const *self.raw
    }

    /// Add one `(format, modifier)` pair.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] on allocation failure, and for
    /// [`FourCc::INVALID`] — `wlr_drm_format_set_add` asserts on that one, and
    /// the assertion is compiled into the shipped library, so it is rejected
    /// here instead of aborting the process.
    pub fn add(&mut self, format: FourCc, modifier: Modifier) -> Result<()> {
        if format == FourCc::INVALID {
            return Err(Error::Operation("wlr_drm_format_set_add"));
        }
        // SAFETY: `raw` is a live, initialised set this value owns exclusively.
        let ok = unsafe { sys::wlr_drm_format_set_add(&raw mut *self.raw, format.0, modifier.0) };
        if ok {
            Ok(())
        } else {
            Err(Error::Operation("wlr_drm_format_set_add"))
        }
    }

    /// Remove one `(format, modifier)` pair. `false` if it was not present.
    pub fn remove(&mut self, format: FourCc, modifier: Modifier) -> bool {
        // SAFETY: as in `add`.
        unsafe { sys::wlr_drm_format_set_remove(&raw mut *self.raw, format.0, modifier.0) }
    }

    /// Whether this set has the pair.
    pub fn has(&self, format: FourCc, modifier: Modifier) -> bool {
        // SAFETY: `raw` is live and only read.
        unsafe { read::has(self.as_ptr(), format, modifier) }
    }

    /// The entry for `format`, if the set has one.
    pub fn get(&self, format: FourCc) -> Option<DrmFormatRef<'_>> {
        // SAFETY: as above; the returned borrow cannot outlive `&self`, so the
        // set cannot be mutated (and the array reallocated) underneath it.
        unsafe { read::get(self.as_ptr(), format) }
    }

    /// Every format in the set.
    pub fn iter(&self) -> impl Iterator<Item = DrmFormatRef<'_>> + '_ {
        let ptr = self.as_ptr();
        // SAFETY: as in `get`.
        (0..self.len()).filter_map(move |i| unsafe { read::nth(ptr, i) })
    }

    /// How many distinct formats the set holds.
    pub fn len(&self) -> usize {
        // SAFETY: `raw` is live and only read.
        unsafe { read::len(self.as_ptr()) }
    }

    /// Whether the set holds no formats at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A borrowed view of this set.
    pub fn as_ref(&self) -> DrmFormatSetRef<'_> {
        // SAFETY: the borrow keeps the set alive and unmutated for `'_`.
        unsafe { DrmFormatSetRef::from_raw(self.as_ptr()) }
    }

    /// The pairs present in both `a` and `b`.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] on failure **and on an empty intersection**.
    /// `wlr_drm_format_set_intersect` reports both as `false` and leaves the
    /// destination untouched either way, so there is nothing here to tell them
    /// apart with. Returning `Ok(empty)` for the false case would be a guess,
    /// and a wrong one whenever the call actually failed.
    pub fn intersect(a: &DrmFormatSet, b: &DrmFormatSet) -> Result<DrmFormatSet> {
        let mut out = DrmFormatSet::new();
        // SAFETY: `out` is a live, zeroed set this call owns exclusively —
        // which is what wlroots requires of the destination, since it
        // `finish`es it before overwriting — and the two sources are live and
        // only read.
        let ok =
            unsafe { sys::wlr_drm_format_set_intersect(&raw mut *out.raw, a.as_ptr(), b.as_ptr()) };
        if ok {
            Ok(out)
        } else {
            Err(Error::Operation("wlr_drm_format_set_intersect"))
        }
    }

    /// Every pair present in either `a` or `b`.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] on allocation failure. Unlike
    /// [`intersect`](DrmFormatSet::intersect) an empty result is not an error
    /// here: the union of two empty sets is reported as success.
    pub fn union(a: &DrmFormatSet, b: &DrmFormatSet) -> Result<DrmFormatSet> {
        let mut out = DrmFormatSet::new();
        // SAFETY: as in `intersect`.
        let ok =
            unsafe { sys::wlr_drm_format_set_union(&raw mut *out.raw, a.as_ptr(), b.as_ptr()) };
        if ok {
            Ok(out)
        } else {
            Err(Error::Operation("wlr_drm_format_set_union"))
        }
    }
}

impl Default for DrmFormatSet {
    fn default() -> DrmFormatSet {
        DrmFormatSet::new()
    }
}

impl std::fmt::Debug for DrmFormatSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl Drop for DrmFormatSet {
    fn drop(&mut self) {
        // SAFETY: `raw` is a live set this value owns; `finish` frees every
        // format and the array, and leaves the set empty and reusable — so it
        // is sound even for a set nothing was ever added to.
        unsafe { sys::wlr_drm_format_set_finish(&raw mut *self.raw) };
    }
}

/// A borrowed set of formats wlroots owns — a renderer's texture formats, most
/// of all.
///
/// No `Drop`: `wlr_drm_format_set_finish` on one of these frees memory the
/// renderer still uses.
#[derive(Clone, Copy)]
pub struct DrmFormatSetRef<'a> {
    raw: *const sys::wlr_drm_format_set,
    _scope: PhantomData<&'a ()>,
}

impl<'a> DrmFormatSetRef<'a> {
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_drm_format_set` that outlives `'a` and is
    /// not mutated for that whole period.
    pub(crate) unsafe fn from_raw(raw: *const sys::wlr_drm_format_set) -> DrmFormatSetRef<'a> {
        DrmFormatSetRef {
            raw,
            _scope: PhantomData,
        }
    }

    /// Whether this set has the pair.
    pub fn has(&self, format: FourCc, modifier: Modifier) -> bool {
        // SAFETY: the borrow guarantees the set is live and unmutated.
        unsafe { read::has(self.raw, format, modifier) }
    }

    /// The entry for `format`, if the set has one.
    pub fn get(&self, format: FourCc) -> Option<DrmFormatRef<'a>> {
        // SAFETY: as above.
        unsafe { read::get(self.raw, format) }
    }

    /// Every format in the set.
    pub fn iter(&self) -> impl Iterator<Item = DrmFormatRef<'a>> + '_ {
        let ptr = self.raw;
        // SAFETY: as above.
        (0..self.len()).filter_map(move |i| unsafe { read::nth(ptr, i) })
    }

    /// How many distinct formats the set holds.
    pub fn len(&self) -> usize {
        // SAFETY: as above.
        unsafe { read::len(self.raw) }
    }

    /// Whether the set holds no formats at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy this set into one Rust owns.
    pub fn to_owned(&self) -> Result<DrmFormatSet> {
        let mut out = DrmFormatSet::new();
        for format in self.iter() {
            for modifier in format.modifiers() {
                out.add(format.format(), modifier)?;
            }
        }
        Ok(out)
    }
}

impl std::fmt::Debug for DrmFormatSetRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three named codes are DRM ABI. Pin them against the characters they
    /// are named for rather than against a hex literal a reader cannot check.
    #[test]
    fn fourcc_codes_pack_least_significant_byte_first() {
        assert_eq!(FourCc::ARGB8888.0, 0x3432_5241);
        assert_eq!(FourCc::XRGB8888.0, 0x3432_5258);
        assert_eq!(FourCc::ABGR8888.0, 0x3432_4241);
    }

    /// `buffer.rs` hand-rolled `DRM_FORMAT_ABGR8888` before this type existed,
    /// and the two must agree — a consumer reading pixels out of a
    /// [`Runtime::add_buffer`](crate::Runtime::add_buffer) buffer uses one of
    /// them to say what the bytes are.
    #[test]
    fn abgr8888_matches_the_buffer_modules_hand_rolled_constant() {
        // Named the buffer module's constant and then compared against a
        // literal instead, so the two could drift apart with the test still
        // green — which is the entire thing it exists to prevent. `buffer.rs`
        // hand-rolls its own copy because it predates this module.
        assert_eq!(FourCc::ABGR8888.0, crate::buffer::DRM_FORMAT_ABGR8888);
    }

    #[test]
    fn fourcc_debug_shows_the_characters_and_the_code() {
        assert_eq!(
            format!("{:?}", FourCc::ARGB8888),
            "FourCc(AR24, 0x34325241)"
        );
        assert_eq!(
            format!("{:?}", FourCc(0x0000_0041)),
            "FourCc(A\\x00\\x00\\x00, 0x00000041)"
        );
    }

    /// The two modifier constants are libdrm ABI (`drm_fourcc.h`), restated
    /// here because the allowlist binds no libdrm symbols.
    #[test]
    fn modifier_constants_match_libdrm() {
        assert_eq!(Modifier::INVALID.0, (1u64 << 56) - 1);
        assert_eq!(Modifier::LINEAR.0, 0);
    }

    #[test]
    fn a_fresh_set_is_empty() {
        let set = DrmFormatSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert_eq!(set.iter().count(), 0);
        assert!(set.get(FourCc::ARGB8888).is_none());
    }

    #[test]
    fn add_has_get_and_remove_agree() {
        let mut set = DrmFormatSet::new();
        set.add(FourCc::ARGB8888, Modifier::LINEAR).unwrap();
        set.add(FourCc::ARGB8888, Modifier::INVALID).unwrap();
        set.add(FourCc::XRGB8888, Modifier::LINEAR).unwrap();

        assert_eq!(set.len(), 2);
        assert!(set.has(FourCc::ARGB8888, Modifier::LINEAR));
        assert!(set.has(FourCc::ARGB8888, Modifier::INVALID));
        assert!(!set.has(FourCc::ABGR8888, Modifier::LINEAR));

        let argb = set.get(FourCc::ARGB8888).expect("added above");
        assert_eq!(argb.format(), FourCc::ARGB8888);
        assert_eq!(
            argb.modifiers().collect::<Vec<_>>(),
            vec![Modifier::LINEAR, Modifier::INVALID]
        );

        assert!(set.remove(FourCc::ARGB8888, Modifier::INVALID));
        assert!(!set.remove(FourCc::ARGB8888, Modifier::INVALID));
        assert!(!set.has(FourCc::ARGB8888, Modifier::INVALID));
    }

    /// The set holds `wlr_drm_format` structs by value, so walking it must add
    /// `index * size_of::<wlr_drm_format>()` rather than dereference a pointer
    /// array. Two distinct formats with distinct modifier lists is the smallest
    /// case where reading it the other way produces garbage instead of a crash.
    #[test]
    fn iteration_reads_the_array_as_structs_by_value() {
        let mut set = DrmFormatSet::new();
        set.add(FourCc::ARGB8888, Modifier::LINEAR).unwrap();
        set.add(FourCc::XRGB8888, Modifier::INVALID).unwrap();

        let seen: Vec<(FourCc, Vec<Modifier>)> = set
            .iter()
            .map(|f| (f.format(), f.modifiers().collect()))
            .collect();
        assert_eq!(
            seen,
            vec![
                (FourCc::ARGB8888, vec![Modifier::LINEAR]),
                (FourCc::XRGB8888, vec![Modifier::INVALID]),
            ]
        );
    }

    #[test]
    fn add_rejects_the_invalid_format_rather_than_tripping_wlroots_assert() {
        let mut set = DrmFormatSet::new();
        assert_eq!(
            set.add(FourCc::INVALID, Modifier::LINEAR),
            Err(Error::Operation("wlr_drm_format_set_add"))
        );
    }

    #[test]
    fn intersect_keeps_only_the_common_pairs() {
        let mut a = DrmFormatSet::new();
        a.add(FourCc::ARGB8888, Modifier::LINEAR).unwrap();
        a.add(FourCc::ARGB8888, Modifier::INVALID).unwrap();
        a.add(FourCc::XRGB8888, Modifier::LINEAR).unwrap();

        let mut b = DrmFormatSet::new();
        b.add(FourCc::ARGB8888, Modifier::LINEAR).unwrap();
        b.add(FourCc::ABGR8888, Modifier::LINEAR).unwrap();

        let out = DrmFormatSet::intersect(&a, &b).expect("ARGB8888/LINEAR is common");
        assert_eq!(out.len(), 1);
        assert!(out.has(FourCc::ARGB8888, Modifier::LINEAR));
        assert!(!out.has(FourCc::ARGB8888, Modifier::INVALID));
    }

    /// The documented wart: an empty intersection is indistinguishable from a
    /// failure, so it is reported as one.
    #[test]
    fn an_empty_intersection_is_reported_as_an_error() {
        let mut a = DrmFormatSet::new();
        a.add(FourCc::ARGB8888, Modifier::LINEAR).unwrap();
        let mut b = DrmFormatSet::new();
        b.add(FourCc::XRGB8888, Modifier::LINEAR).unwrap();

        assert_eq!(
            DrmFormatSet::intersect(&a, &b).err(),
            Some(Error::Operation("wlr_drm_format_set_intersect"))
        );
    }

    #[test]
    fn union_merges_both_sides() {
        let mut a = DrmFormatSet::new();
        a.add(FourCc::ARGB8888, Modifier::LINEAR).unwrap();
        let mut b = DrmFormatSet::new();
        b.add(FourCc::ARGB8888, Modifier::INVALID).unwrap();
        b.add(FourCc::XRGB8888, Modifier::LINEAR).unwrap();

        let out = DrmFormatSet::union(&a, &b).expect("union of two live sets");
        assert_eq!(out.len(), 2);
        assert!(out.has(FourCc::ARGB8888, Modifier::LINEAR));
        assert!(out.has(FourCc::ARGB8888, Modifier::INVALID));
        assert!(out.has(FourCc::XRGB8888, Modifier::LINEAR));
    }

    #[test]
    fn a_borrowed_view_reads_the_same_answers() {
        let mut set = DrmFormatSet::new();
        set.add(FourCc::ARGB8888, Modifier::LINEAR).unwrap();
        let view = set.as_ref();
        assert_eq!(view.len(), set.len());
        assert!(view.has(FourCc::ARGB8888, Modifier::LINEAR));
        assert_eq!(view.get(FourCc::ARGB8888).unwrap().modifiers().count(), 1);
        let copied = view.to_owned().expect("copy of a two-entry set");
        assert!(copied.has(FourCc::ARGB8888, Modifier::LINEAR));
    }

    /// `with_sys` must hand wlroots a format whose `capacity` is at least its
    /// `len` (`wlr_drm_format_copy` asserts it) and whose modifiers are the
    /// ones the `DrmFormat` holds.
    #[test]
    fn with_sys_materialises_a_readable_format() {
        let fmt = DrmFormat::new(FourCc::ARGB8888, [Modifier::LINEAR, Modifier::INVALID]);
        fmt.with_sys(|raw| {
            // SAFETY: `raw` points at the temporary `with_sys` built, live for
            // the duration of this closure.
            unsafe {
                assert_eq!((*raw).format, FourCc::ARGB8888.0);
                assert_eq!((*raw).len, 2);
                assert!((*raw).capacity >= (*raw).len);
                let mods = std::slice::from_raw_parts((*raw).modifiers, (*raw).len);
                assert_eq!(mods, [Modifier::LINEAR.0, Modifier::INVALID.0]);
            }
        });
    }
}
