//! Colour spaces, colour encodings and colour transforms.
//!
//! `wlr/render/color.h` is the vocabulary the rest of the render API speaks:
//! [`TextureOptions`](super::TextureOptions) tags its source with a transfer
//! function and an encoding, [`BufferPassOptions`](super::BufferPassOptions)
//! applies a [`ColorTransform`] to the pass output, and wlroots' colour
//! management protocols (a later milestone) are built on the same enums.
//!
//! # Bitmask enums that are also single values
//!
//! Four of these C enums are **bitmask-valued** —
//! `wlr_color_named_primaries`, `wlr_color_transfer_function` and
//! `wlr_color_encoding` number their variants `1 << n` — and yet each is used
//! in two different roles. `wlr_renderer.color_encodings` is a *mask* of
//! `wlr_color_encoding`, while `wlr_render_texture_options.color_encoding` is a
//! *single* `wlr_color_encoding`. Conflating the two would let a consumer OR
//! two encodings together and hand the result to a field that means one.
//!
//! So each role gets its own type: [`ColorEncoding`] and [`TransferFunction`]
//! are single values, [`ColorEncodings`] and [`TransferFunctions`] are sets.
//! The bit values themselves are pinned by `tests/render_color.rs`, because
//! "sequential or bitmask?" is exactly the question a reader cannot answer by
//! looking at the Rust.
//!
//! # Zero means "unset", and not always legally
//!
//! `wlr_color_encoding`, `wlr_color_range` and `wlr_color_chroma_location` all
//! have a `NONE = 0` variant, which is what wlroots reads out of a zeroed
//! options struct. `wlr_color_transfer_function` has **no zero variant at
//! all**: its unset state is the integer 0, which is not a value of the enum.
//! [`TransferFunction`] therefore has no `Default` and the pass options carry
//! it as an `Option`.

use std::ptr::NonNull;

use crate::error::{Error, Result};
use crate::sys;

/// A well-known set of colour primaries.
///
/// The C enum is bitmask-valued (`SRGB = 1 << 0`, `BT2020 = 1 << 1`) so that
/// wlroots' colour-management protocol code can OR them into a capability
/// list; as an argument to [`ColorPrimaries::named`] it is a single value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NamedPrimaries {
    /// sRGB / BT.709 primaries (H.273 code point 1).
    Srgb,
    /// BT.2020 primaries (H.273 code point 9).
    Bt2020,
}

impl From<NamedPrimaries> for sys::wlr_color_named_primaries {
    fn from(p: NamedPrimaries) -> sys::wlr_color_named_primaries {
        match p {
            NamedPrimaries::Srgb => sys::wlr_color_named_primaries::WLR_COLOR_NAMED_PRIMARIES_SRGB,
            NamedPrimaries::Bt2020 => {
                sys::wlr_color_named_primaries::WLR_COLOR_NAMED_PRIMARIES_BT2020
            }
        }
    }
}

/// A well-known electro-optical transfer function.
///
/// Deliberately has no [`Default`]: the C enum's variants start at `1 << 0`,
/// so there is no value of it that means "unset" and a zero in that field is
/// not a member of the enum at all. Where wlroots accepts "unset", this crate
/// spells it `Option<TransferFunction>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TransferFunction {
    /// The sRGB piecewise curve.
    Srgb,
    /// SMPTE ST 2084 (PQ), the HDR10 curve.
    St2084Pq,
    /// Extended linear.
    ExtLinear,
    /// A pure gamma 2.2 power curve.
    Gamma22,
    /// BT.1886.
    Bt1886,
}

impl From<TransferFunction> for sys::wlr_color_transfer_function {
    fn from(tf: TransferFunction) -> sys::wlr_color_transfer_function {
        use sys::wlr_color_transfer_function as C;
        match tf {
            TransferFunction::Srgb => C::WLR_COLOR_TRANSFER_FUNCTION_SRGB,
            TransferFunction::St2084Pq => C::WLR_COLOR_TRANSFER_FUNCTION_ST2084_PQ,
            TransferFunction::ExtLinear => C::WLR_COLOR_TRANSFER_FUNCTION_EXT_LINEAR,
            TransferFunction::Gamma22 => C::WLR_COLOR_TRANSFER_FUNCTION_GAMMA22,
            TransferFunction::Bt1886 => C::WLR_COLOR_TRANSFER_FUNCTION_BT1886,
        }
    }
}

/// The matrix coefficients converting a YCbCr encoding to RGB.
///
/// [`ColorEncoding::None`] is wlroots' "unset or unknown", and is what a zeroed
/// options struct reads as — which is why it is the [`Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ColorEncoding {
    /// Unset or unknown.
    #[default]
    None,
    /// Identity (the buffer is already RGB).
    Identity,
    /// BT.709.
    Bt709,
    /// FCC.
    Fcc,
    /// BT.601.
    Bt601,
    /// SMPTE 240M.
    Smpte240,
    /// BT.2020 non-constant luminance.
    Bt2020,
    /// BT.2020 constant luminance.
    Bt2020Cl,
    /// ICtCp.
    Ictcp,
}

impl From<ColorEncoding> for sys::wlr_color_encoding {
    fn from(e: ColorEncoding) -> sys::wlr_color_encoding {
        use sys::wlr_color_encoding as C;
        match e {
            ColorEncoding::None => C::WLR_COLOR_ENCODING_NONE,
            ColorEncoding::Identity => C::WLR_COLOR_ENCODING_IDENTITY,
            ColorEncoding::Bt709 => C::WLR_COLOR_ENCODING_BT709,
            ColorEncoding::Fcc => C::WLR_COLOR_ENCODING_FCC,
            ColorEncoding::Bt601 => C::WLR_COLOR_ENCODING_BT601,
            ColorEncoding::Smpte240 => C::WLR_COLOR_ENCODING_SMPTE240,
            ColorEncoding::Bt2020 => C::WLR_COLOR_ENCODING_BT2020,
            ColorEncoding::Bt2020Cl => C::WLR_COLOR_ENCODING_BT2020_CL,
            ColorEncoding::Ictcp => C::WLR_COLOR_ENCODING_ICTCP,
        }
    }
}

/// Whether an encoding uses the full or the limited value range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ColorRange {
    /// Unset or unknown.
    #[default]
    None,
    /// Limited ("TV") range.
    Limited,
    /// Full ("PC") range.
    Full,
}

impl From<ColorRange> for sys::wlr_color_range {
    fn from(r: ColorRange) -> sys::wlr_color_range {
        match r {
            ColorRange::None => sys::wlr_color_range::WLR_COLOR_RANGE_NONE,
            ColorRange::Limited => sys::wlr_color_range::WLR_COLOR_RANGE_LIMITED,
            ColorRange::Full => sys::wlr_color_range::WLR_COLOR_RANGE_FULL,
        }
    }
}

/// Where a chroma sample sits relative to its luma samples.
///
/// The H.273 `Chroma420SampleLocType` code points, which is why they are
/// numbered rather than named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ChromaLocation {
    /// Unset or unknown.
    #[default]
    None,
    /// Code point 0.
    Type0,
    /// Code point 1.
    Type1,
    /// Code point 2.
    Type2,
    /// Code point 3.
    Type3,
    /// Code point 4.
    Type4,
    /// Code point 5.
    Type5,
}

impl From<ChromaLocation> for sys::wlr_color_chroma_location {
    fn from(c: ChromaLocation) -> sys::wlr_color_chroma_location {
        use sys::wlr_color_chroma_location as C;
        match c {
            ChromaLocation::None => C::WLR_COLOR_CHROMA_LOCATION_NONE,
            ChromaLocation::Type0 => C::WLR_COLOR_CHROMA_LOCATION_TYPE0,
            ChromaLocation::Type1 => C::WLR_COLOR_CHROMA_LOCATION_TYPE1,
            ChromaLocation::Type2 => C::WLR_COLOR_CHROMA_LOCATION_TYPE2,
            ChromaLocation::Type3 => C::WLR_COLOR_CHROMA_LOCATION_TYPE3,
            ChromaLocation::Type4 => C::WLR_COLOR_CHROMA_LOCATION_TYPE4,
            ChromaLocation::Type5 => C::WLR_COLOR_CHROMA_LOCATION_TYPE5,
        }
    }
}

/// How alpha has been applied to the colour channels.
///
/// Unlike every other enum here there is no "unset": wlroots' own header says
/// premultiplied-electrical is the default, so that is the [`Default`] and a
/// zeroed struct already means it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum AlphaMode {
    /// Premultiplied in the electrical (encoded) domain. wlroots' default.
    #[default]
    PremultipliedElectrical,
    /// Premultiplied in the optical (linear) domain.
    PremultipliedOptical,
    /// Not premultiplied.
    Straight,
}

impl From<AlphaMode> for sys::wlr_alpha_mode {
    fn from(m: AlphaMode) -> sys::wlr_alpha_mode {
        use sys::wlr_alpha_mode as C;
        match m {
            AlphaMode::PremultipliedElectrical => C::WLR_COLOR_ALPHA_MODE_PREMULTIPLIED_ELECTRICAL,
            AlphaMode::PremultipliedOptical => C::WLR_COLOR_ALPHA_MODE_PREMULTIPLIED_OPTICAL,
            AlphaMode::Straight => C::WLR_COLOR_ALPHA_MODE_STRAIGHT,
        }
    }
}

/// A set of [`ColorEncoding`]s.
///
/// The bitmask companion to the single-valued enum, which is the role
/// `wlr_renderer.color_encodings` uses the same C enum in. Hand-rolled rather
/// than a `bitflags` dependency, following [`Interest`](crate::Interest).
///
/// [`ColorEncoding::None`] is the integer zero, so it is not a member of any
/// set — `contains(ColorEncoding::None)` is vacuously true and says nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ColorEncodings(u32);

impl ColorEncodings {
    /// The empty set.
    pub const NONE: ColorEncodings = ColorEncodings(0);

    /// Build a set from a raw `wlr_color_encoding` mask.
    pub fn from_bits(bits: u32) -> ColorEncodings {
        ColorEncodings(bits)
    }

    /// The raw mask.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Whether `encoding`'s bit is set.
    pub fn contains(self, encoding: ColorEncoding) -> bool {
        let bit = sys::wlr_color_encoding::from(encoding).0;
        self.0 & bit == bit
    }

    /// This set with `encoding` added.
    #[must_use]
    pub fn with(self, encoding: ColorEncoding) -> ColorEncodings {
        ColorEncodings(self.0 | sys::wlr_color_encoding::from(encoding).0)
    }

    /// Whether no bit is set.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// A set of [`TransferFunction`]s.
///
/// The bitmask companion to the single-valued enum. wlroots' colour-management
/// code builds capability lists this way; nothing in the render API takes one,
/// which is why it has constructors and no wlroots-facing conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TransferFunctions(u32);

impl TransferFunctions {
    /// The empty set.
    pub const NONE: TransferFunctions = TransferFunctions(0);

    /// Build a set from a raw `wlr_color_transfer_function` mask.
    pub fn from_bits(bits: u32) -> TransferFunctions {
        TransferFunctions(bits)
    }

    /// The raw mask.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Whether `tf`'s bit is set.
    pub fn contains(self, tf: TransferFunction) -> bool {
        let bit = sys::wlr_color_transfer_function::from(tf).0;
        self.0 & bit == bit
    }

    /// This set with `tf` added.
    #[must_use]
    pub fn with(self, tf: TransferFunction) -> TransferFunctions {
        TransferFunctions(self.0 | sys::wlr_color_transfer_function::from(tf).0)
    }

    /// Whether no bit is set.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// CIE 1931 xy chromaticity coordinates: `wlr_color_cie1931_xy`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Cie1931Xy {
    /// The x coordinate.
    pub x: f32,
    /// The y coordinate.
    pub y: f32,
}

const _: () = assert!(size_of::<Cie1931Xy>() == size_of::<sys::wlr_color_cie1931_xy>());
const _: () = assert!(align_of::<Cie1931Xy>() == align_of::<sys::wlr_color_cie1931_xy>());
const _: () = assert!(
    std::mem::offset_of!(Cie1931Xy, x) == std::mem::offset_of!(sys::wlr_color_cie1931_xy, x)
);
const _: () = assert!(
    std::mem::offset_of!(Cie1931Xy, y) == std::mem::offset_of!(sys::wlr_color_cie1931_xy, y)
);

/// The primaries and white point describing a colour volume:
/// `wlr_color_primaries`.
///
/// `#[repr(C)]` and pinned to wlroots' layout below, so handing one to C is a
/// pointer cast rather than a conversion.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ColorPrimaries {
    /// The red primary.
    pub red: Cie1931Xy,
    /// The green primary.
    pub green: Cie1931Xy,
    /// The blue primary.
    pub blue: Cie1931Xy,
    /// The white point.
    pub white: Cie1931Xy,
}

const _: () = assert!(size_of::<ColorPrimaries>() == size_of::<sys::wlr_color_primaries>());
const _: () = assert!(align_of::<ColorPrimaries>() == align_of::<sys::wlr_color_primaries>());
const _: () = assert!(
    std::mem::offset_of!(ColorPrimaries, red)
        == std::mem::offset_of!(sys::wlr_color_primaries, red)
);
const _: () = assert!(
    std::mem::offset_of!(ColorPrimaries, green)
        == std::mem::offset_of!(sys::wlr_color_primaries, green)
);
const _: () = assert!(
    std::mem::offset_of!(ColorPrimaries, blue)
        == std::mem::offset_of!(sys::wlr_color_primaries, blue)
);
const _: () = assert!(
    std::mem::offset_of!(ColorPrimaries, white)
        == std::mem::offset_of!(sys::wlr_color_primaries, white)
);

impl ColorPrimaries {
    /// The primaries of a well-known colour volume.
    pub fn named(p: NamedPrimaries) -> ColorPrimaries {
        let mut out = ColorPrimaries::default();
        // SAFETY: `out` is a live, exclusively-owned local whose layout is
        // pinned to `wlr_color_primaries` above, and this call only writes to
        // it. `named` is one of the two values the C `switch` handles — every
        // other input `abort()`s, which is why the argument is an enum this
        // crate controls rather than an integer.
        unsafe {
            sys::wlr_color_primaries_from_named(
                (&raw mut out).cast::<sys::wlr_color_primaries>(),
                p.into(),
            );
        }
        out
    }

    /// This colour volume as the C type, for the duration of the borrow.
    pub(crate) fn as_c(&self) -> *const sys::wlr_color_primaries {
        (&raw const *self).cast::<sys::wlr_color_primaries>()
    }

    /// The 3×3 matrix converting linear RGB in this volume to linear RGB in
    /// `dst`, preserving absolute luminance.
    ///
    /// **Row-major**, matching wlroots: `wlr_color_transform_init_matrix`'s
    /// evaluator computes `out[0] = m[0]*v[0] + m[1]*v[1] + m[2]*v[2]`
    /// (`multiply_matrix_vector` in `render/color.c`), so elements 0..3 are the
    /// first row. Feeding a column-major matrix through
    /// [`ColorTransform::matrix`] transposes the transform silently.
    pub fn transform_absolute_colorimetric(&self, dst: &ColorPrimaries) -> [f32; 9] {
        let mut matrix = [0.0f32; 9];
        // SAFETY: both operands are live for the call and pinned to
        // `wlr_color_primaries`' layout; `matrix` is a live local of exactly
        // the nine floats wlroots writes. The function is pure — it retains
        // nothing.
        unsafe {
            sys::wlr_color_primaries_transform_absolute_colorimetric(
                self.as_c(),
                dst.as_c(),
                matrix.as_mut_ptr(),
            );
        }
        matrix
    }
}

/// A luminance range and reference white level, in cd/m²:
/// `wlr_color_luminances`.
///
/// Plain data. Nothing in the render API consumes one — it is the vocabulary
/// wlroots' colour-management protocols are written in, wrapped here because
/// the type belongs with the rest of `render/color.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ColorLuminances {
    /// The darkest luminance the content uses.
    pub min: f32,
    /// The brightest luminance the content uses.
    pub max: f32,
    /// The reference white level.
    pub reference: f32,
}

const _: () = assert!(size_of::<ColorLuminances>() == size_of::<sys::wlr_color_luminances>());
const _: () = assert!(align_of::<ColorLuminances>() == align_of::<sys::wlr_color_luminances>());
const _: () = assert!(
    std::mem::offset_of!(ColorLuminances, min)
        == std::mem::offset_of!(sys::wlr_color_luminances, min)
);
const _: () = assert!(
    std::mem::offset_of!(ColorLuminances, max)
        == std::mem::offset_of!(sys::wlr_color_luminances, max)
);
const _: () = assert!(
    std::mem::offset_of!(ColorLuminances, reference)
        == std::mem::offset_of!(sys::wlr_color_luminances, reference)
);

/// An immutable, reference-counted colour transform.
///
/// Maps linear light with sRGB primaries into some output colour space.
/// wlroots allocates these on the heap with an initial reference count of 1 and
/// provides no way to modify one after construction, so [`Clone`] is a genuine
/// reference (`wlr_color_transform_ref`) rather than a copy, and [`Drop`] is
/// `wlr_color_transform_unref`.
///
/// A transform is independent of any renderer: it may be built before one
/// exists and outlive the renderer a pass applied it with.
pub struct ColorTransform {
    raw: NonNull<sys::wlr_color_transform>,
}

impl ColorTransform {
    /// Wrap a transform whose reference this value takes over.
    fn adopt(raw: *mut sys::wlr_color_transform, what: &'static str) -> Result<ColorTransform> {
        Ok(ColorTransform {
            raw: NonNull::new(raw).ok_or(Error::Create(what))?,
        })
    }

    /// A transform from linear light to the colour space of an ICC profile.
    ///
    /// `profile` is the raw ICC profile, which wlroots copies.
    ///
    /// Present only when the installed wlroots was built with lcms2
    /// (`wlr_has_color_management`); without it the C function exists but can
    /// only ever return null, and a method that is unconditionally an error is
    /// worse than one that is not there.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if lcms2 could not parse `profile` or the allocation
    /// failed.
    #[cfg(wlr_has_color_management)]
    pub fn from_icc(profile: &[u8]) -> Result<ColorTransform> {
        // SAFETY: `profile` is live for the call and wlroots copies out of it
        // (`render/color_lcms2.c` hands it to `cmsOpenProfileFromMem`, which
        // parses eagerly); the length is the slice's own.
        let raw = unsafe {
            sys::wlr_color_transform_init_linear_to_icc(profile.as_ptr().cast(), profile.len())
        };
        ColorTransform::adopt(raw, "wlr_color_transform_init_linear_to_icc")
    }

    /// A transform applying `tf`'s inverse EOTF — the encoding direction.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if the allocation failed.
    pub fn inverse_eotf(tf: TransferFunction) -> Result<ColorTransform> {
        // SAFETY: no pointers cross this boundary, and `tf` is one of the five
        // values the C evaluator's `switch` handles — every other input
        // `abort()`s, which is why this takes an enum and not an integer.
        let raw = unsafe { sys::wlr_color_transform_init_linear_to_inverse_eotf(tf.into()) };
        ColorTransform::adopt(raw, "wlr_color_transform_init_linear_to_inverse_eotf")
    }

    /// A transform applying one 1-D lookup table per channel.
    ///
    /// The three slices are the red, green and blue tables and must all be the
    /// same, non-zero length; that length is wlroots' `dim`. Entries are
    /// unsigned 16-bit fractions of full scale, interpolated linearly.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] — without calling wlroots — if the lengths differ
    /// or any is empty. Neither is checked by C: mismatched lengths make
    /// `wlr_color_transform_init_lut_3x1d` read past the end of the short
    /// slices, and a `dim` of 0 makes its evaluator compute `len - 1` in
    /// `size_t` and index the table at `SIZE_MAX` (`lut_1d_get` in
    /// `render/color.c`). [`Error::Create`] if the allocation failed.
    pub fn lut_3x1d(r: &[u16], g: &[u16], b: &[u16]) -> Result<ColorTransform> {
        if r.is_empty() || r.len() != g.len() || r.len() != b.len() {
            return Err(Error::Operation("ColorTransform::lut_3x1d"));
        }
        // SAFETY: the three slices are live for the call and all hold at least
        // `r.len()` entries (checked above), which is the `dim` wlroots reads
        // that many from each of. wlroots copies them into its own allocation.
        let raw = unsafe {
            sys::wlr_color_transform_init_lut_3x1d(r.len(), r.as_ptr(), g.as_ptr(), b.as_ptr())
        };
        ColorTransform::adopt(raw, "wlr_color_transform_init_lut_3x1d")
    }

    /// A transform applying a 3×3 matrix.
    ///
    /// **Row-major**: `m[0..3]` is the first row, so the transform computes
    /// `out[0] = m[0]*r + m[1]*g + m[2]*b`. Verified against
    /// `multiply_matrix_vector` in `render/color.c` rather than assumed — the
    /// header says only "a 3×3 matrix", and a column-major matrix passed here
    /// transposes the transform without failing.
    ///
    /// [`ColorPrimaries::transform_absolute_colorimetric`] produces a matrix in
    /// exactly this order.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if the allocation failed.
    pub fn matrix(m: [f32; 9]) -> Result<ColorTransform> {
        // SAFETY: `m` is a live local of exactly the nine floats wlroots reads
        // and copies.
        let raw = unsafe { sys::wlr_color_transform_init_matrix(m.as_ptr()) };
        ColorTransform::adopt(raw, "wlr_color_transform_init_matrix")
    }

    /// A transform applying `inputs` one after another, left to right.
    ///
    /// wlroots takes its own reference on every input
    /// (`wlr_color_transform_init_pipeline` calls `wlr_color_transform_ref` on
    /// each — verified in `render/color.c`), so the pipeline keeps them alive
    /// on its own and this value stores nothing extra.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] — without calling wlroots — for an empty `inputs`,
    /// which `wlr_color_transform_init_pipeline` **asserts** against and the
    /// distro build of wlroots aborts on. [`Error::Create`] if the allocation
    /// failed.
    pub fn pipeline(inputs: &[ColorTransform]) -> Result<ColorTransform> {
        if inputs.is_empty() {
            return Err(Error::Operation("ColorTransform::pipeline"));
        }
        let mut ptrs: Vec<*mut sys::wlr_color_transform> =
            inputs.iter().map(ColorTransform::as_ptr).collect();
        // SAFETY: `ptrs` is a live array of exactly `inputs.len()` non-null
        // pointers to live transforms — each `inputs[i]` holds a reference for
        // the duration of this call — which satisfies both of the assertions
        // the C function makes. It reads the array and does not retain it.
        let raw = unsafe { sys::wlr_color_transform_init_pipeline(ptrs.as_mut_ptr(), ptrs.len()) };
        ColorTransform::adopt(raw, "wlr_color_transform_init_pipeline")
    }

    /// Evaluate this transform on one RGB triplet, on the CPU.
    pub fn eval(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut out = [0.0f32; 3];
        // SAFETY: this value holds a reference to a live transform, and both
        // arrays are live locals of exactly the three floats wlroots reads and
        // writes.
        unsafe {
            sys::wlr_color_transform_eval(self.raw.as_ptr(), out.as_mut_ptr(), rgb.as_ptr());
        }
        out
    }

    /// The raw transform, for interop with code that speaks `wlr-sys` directly.
    ///
    /// Borrowed: this value still holds the reference it will release on drop.
    pub fn as_ptr(&self) -> *mut sys::wlr_color_transform {
        self.raw.as_ptr()
    }
}

impl Clone for ColorTransform {
    /// Takes another reference. Transforms are immutable, so a clone and its
    /// original are indistinguishable — including by pointer.
    fn clone(&self) -> ColorTransform {
        // SAFETY: this value holds a live reference, so the count is at least
        // one and incrementing it cannot resurrect a freed object.
        let raw = unsafe { sys::wlr_color_transform_ref(self.raw.as_ptr()) };
        ColorTransform {
            raw: NonNull::new(raw).expect("wlr_color_transform_ref returns its argument"),
        }
    }
}

impl std::fmt::Debug for ColorTransform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The type is opaque and its reference count is behind wlroots'
        // `WLR_PRIVATE`, so the pointer identity is the only honest field.
        f.debug_struct("ColorTransform")
            .field("raw", &self.raw)
            .finish()
    }
}

impl Drop for ColorTransform {
    fn drop(&mut self) {
        // SAFETY: this value holds exactly one reference, taken by whichever
        // constructor or `clone` produced it, and releases it exactly once
        // here. A pipeline built from this transform holds its own reference,
        // so dropping an input while a pipeline still names it is safe.
        unsafe { sys::wlr_color_transform_unref(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every colour type here is `!Send`/`!Sync` or plain data, and which is
    /// which is not incidental: `ColorTransform`'s reference count is a bare
    /// `size_t` wlroots increments without a lock, so sharing one across
    /// threads would race the count itself.
    #[test]
    fn a_color_transform_does_not_cross_a_thread_but_the_value_types_do() {
        use static_assertions::{assert_impl_all, assert_not_impl_any};

        assert_not_impl_any!(ColorTransform: Send, Sync);
        assert_impl_all!(ColorPrimaries: Send, Sync);
        assert_impl_all!(Cie1931Xy: Send, Sync);
        assert_impl_all!(ColorLuminances: Send, Sync);
    }

    /// The identity matrix has to leave a colour alone, which is the cheapest
    /// end-to-end proof that the matrix constructor and `eval` agree about
    /// element order.
    #[test]
    fn an_identity_matrix_transform_is_the_identity() {
        #[rustfmt::skip]
        let identity = [
            1.0, 0.0, 0.0,
            0.0, 1.0, 0.0,
            0.0, 0.0, 1.0,
        ];
        let tr = ColorTransform::matrix(identity).expect("matrix transform");
        let out = tr.eval([0.25, 0.5, 0.75]);
        for (got, want) in out.iter().zip([0.25f32, 0.5, 0.75]) {
            assert!((got - want).abs() < 1e-6, "{out:?}");
        }
    }

    /// Row-major, and the test has to be able to tell the difference: a matrix
    /// that is not its own transpose gives different answers under the two
    /// conventions, which the identity above cannot.
    #[test]
    fn a_matrix_transform_is_row_major() {
        #[rustfmt::skip]
        let m = [
            0.0, 1.0, 0.0,
            0.0, 0.0, 1.0,
            1.0, 0.0, 0.0,
        ];
        let tr = ColorTransform::matrix(m).expect("matrix transform");
        // Row-major: out[0] = m[0]*r + m[1]*g + m[2]*b = g, out[1] = b,
        // out[2] = r. Column-major would answer [b, r, g].
        let out = tr.eval([1.0, 2.0, 3.0]);
        assert_eq!(out, [2.0, 3.0, 1.0]);
    }

    /// Mismatched or empty lookup tables must be refused in Rust: wlroots reads
    /// `dim` entries out of all three slices whatever their real lengths are,
    /// and a `dim` of 0 indexes at `SIZE_MAX`.
    #[test]
    fn a_malformed_lookup_table_is_refused_before_wlroots_sees_it() {
        let long = [0u16, 1, 2, 3];
        let short = [0u16, 1];
        assert_eq!(
            ColorTransform::lut_3x1d(&long, &short, &long).unwrap_err(),
            Error::Operation("ColorTransform::lut_3x1d")
        );
        assert_eq!(
            ColorTransform::lut_3x1d(&[], &[], &[]).unwrap_err(),
            Error::Operation("ColorTransform::lut_3x1d")
        );
        assert!(ColorTransform::lut_3x1d(&long, &long, &long).is_ok());
    }

    /// `wlr_color_transform_init_pipeline` asserts a non-zero length, and the
    /// distro build of wlroots aborts on a failed assertion rather than
    /// returning.
    #[test]
    fn an_empty_pipeline_is_refused_rather_than_aborting_the_process() {
        assert_eq!(
            ColorTransform::pipeline(&[]).unwrap_err(),
            Error::Operation("ColorTransform::pipeline")
        );
    }
}
