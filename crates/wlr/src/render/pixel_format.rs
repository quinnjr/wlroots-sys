//! How many bytes one row of pixels occupies.
//!
//! # Why this table has to exist here
//!
//! wlroots knows the answer — `render/pixel_format.c` holds the table and
//! `pixel_format_info_min_stride` does the arithmetic — but both are private to
//! its own build: no header installs them, so nothing binds them.
//!
//! That would be a curiosity if a stride were only a hint. It is not. Neither
//! `wlr_texture_from_pixels` nor `wlr_texture_read_pixels` checks the caller's
//! `stride` against the caller's `width`, and both hand the pair straight to a
//! renderer that lays a `pixman_image` (or a `glReadPixels` row walk) over the
//! caller's memory with `width` pixels per row. A `stride` smaller than
//! `width` pixels' worth of bytes therefore reads — or **writes** — past the
//! end of the buffer, while `rows * stride` bytes still look like plenty.
//! Verified rather than argued: a `read_pixels` of an 8×8 `ARGB8888` texture
//! with `stride = 4` into a 32-byte slice clobbers 28 bytes past its end.
//!
//! So the byte count a row really needs is the thing to bound against, and this
//! module is wlroots' own table transcribed to answer it. The `fourcc` codes
//! are `libdrm`'s (`drm_fourcc.h`), spelled with the characters the format's
//! name is written with rather than as hex, so a reader can check each row
//! against the header without decoding anything.

use super::format::FourCc;

/// An upper bound on `bytes_per_block` over every entry in the table below —
/// `ABGR32323232F`, at 16 bytes for one pixel. Used to charge a format the
/// table does not list, where over-charging can only refuse a call that would
/// have fitted.
pub(super) const MAX_BYTES_PER_BLOCK: u64 = 16;

/// One row of wlroots' `pixel_format_info`: the format, the bytes one block
/// occupies, and how many pixels a block covers.
///
/// Only `bytes_per_block` and the pixels-per-block product matter here;
/// wlroots' `opaque_substitute` is about alpha, not size, and is not
/// transcribed.
struct Info {
    format: FourCc,
    bytes_per_block: u64,
    pixels_per_block: u64,
}

/// A single-pixel-per-block entry, which is all but two of them.
const fn packed(a: u8, b: u8, c: u8, d: u8, bytes: u64) -> Info {
    Info {
        format: FourCc::from_chars(a, b, c, d),
        bytes_per_block: bytes,
        pixels_per_block: 1,
    }
}

/// `render/pixel_format.c`'s table, in its own order so the two can be diffed.
const TABLE: &[Info] = &[
    packed(b'X', b'R', b'2', b'4', 4),  // XRGB8888
    packed(b'A', b'R', b'2', b'4', 4),  // ARGB8888
    packed(b'X', b'B', b'2', b'4', 4),  // XBGR8888
    packed(b'A', b'B', b'2', b'4', 4),  // ABGR8888
    packed(b'R', b'X', b'2', b'4', 4),  // RGBX8888
    packed(b'R', b'A', b'2', b'4', 4),  // RGBA8888
    packed(b'B', b'X', b'2', b'4', 4),  // BGRX8888
    packed(b'B', b'A', b'2', b'4', 4),  // BGRA8888
    packed(b'R', b'8', b' ', b' ', 1),  // R8
    packed(b'R', b' ', b' ', b'H', 2),  // R16F
    packed(b'R', b' ', b' ', b'F', 4),  // R32F
    packed(b'G', b'R', b'8', b'8', 2),  // GR88
    packed(b'G', b'R', b' ', b'H', 4),  // GR1616F
    packed(b'G', b'R', b' ', b'F', 8),  // GR3232F
    packed(b'R', b'G', b'2', b'4', 3),  // RGB888
    packed(b'B', b'G', b'2', b'4', 3),  // BGR888
    packed(b'B', b'G', b'4', b'8', 6),  // BGR161616
    packed(b'B', b'G', b'R', b'H', 6),  // BGR161616F
    packed(b'B', b'G', b'R', b'F', 12), // BGR323232F
    packed(b'R', b'X', b'1', b'2', 2),  // RGBX4444
    packed(b'R', b'A', b'1', b'2', 2),  // RGBA4444
    packed(b'B', b'X', b'1', b'2', 2),  // BGRX4444
    packed(b'B', b'A', b'1', b'2', 2),  // BGRA4444
    packed(b'R', b'X', b'1', b'5', 2),  // RGBX5551
    packed(b'R', b'A', b'1', b'5', 2),  // RGBA5551
    packed(b'B', b'X', b'1', b'5', 2),  // BGRX5551
    packed(b'B', b'A', b'1', b'5', 2),  // BGRA5551
    packed(b'X', b'R', b'1', b'5', 2),  // XRGB1555
    packed(b'A', b'R', b'1', b'5', 2),  // ARGB1555
    packed(b'R', b'G', b'1', b'6', 2),  // RGB565
    packed(b'B', b'G', b'1', b'6', 2),  // BGR565
    packed(b'X', b'R', b'3', b'0', 4),  // XRGB2101010
    packed(b'A', b'R', b'3', b'0', 4),  // ARGB2101010
    packed(b'X', b'B', b'3', b'0', 4),  // XBGR2101010
    packed(b'A', b'B', b'3', b'0', 4),  // ABGR2101010
    packed(b'X', b'B', b'4', b'H', 8),  // XBGR16161616F
    packed(b'A', b'B', b'4', b'H', 8),  // ABGR16161616F
    packed(b'A', b'B', b'8', b'F', 16), // ABGR32323232F
    packed(b'X', b'B', b'4', b'8', 8),  // XBGR16161616
    packed(b'A', b'B', b'4', b'8', 8),  // ABGR16161616
    // The two sub-sampled entries: one four-byte block carries two pixels.
    Info {
        format: FourCc::from_chars(b'Y', b'V', b'Y', b'U'),
        bytes_per_block: 4,
        pixels_per_block: 2,
    },
    Info {
        format: FourCc::from_chars(b'V', b'Y', b'U', b'Y'),
        bytes_per_block: 4,
        pixels_per_block: 2,
    },
];

/// The fewest bytes a row of `width` pixels of `format` occupies, or `None` for
/// a format wlroots' own table does not list.
///
/// Rounds up exactly as wlroots' `div_round_up` does rather than rounding to a
/// whole block, because this same number is what wlroots offsets a destination
/// `dst_x` by and the two have to agree. The difference only shows on the two
/// sub-sampled entries at an odd width, and no renderer reaches memory with
/// one: both pixman and GLES2 refuse a `YVYU`/`VYUY` read-back before touching
/// the destination, having no pixel format to map it to.
pub(crate) fn min_stride(format: FourCc, width: u64) -> Option<u64> {
    let info = TABLE.iter().find(|info| info.format == format)?;
    Some(
        width
            .saturating_mul(info.bytes_per_block)
            .div_ceil(info.pixels_per_block),
    )
}

/// The same, over-estimated at [`MAX_BYTES_PER_BLOCK`] per pixel for a format
/// the table does not list.
///
/// Every renderer refuses a format it does not know before touching the
/// caller's memory, so the over-estimate is only ever reached for a call that
/// was going to fail anyway — or for a format a later wlroots patch release
/// added, where refusing it is the safe half of being wrong.
pub(super) fn min_stride_bound(format: FourCc, width: u64) -> u64 {
    min_stride(format, width).unwrap_or_else(|| width.saturating_mul(MAX_BYTES_PER_BLOCK))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three formats this crate names by hand, against arithmetic a reader
    /// can do in their head.
    #[test]
    fn the_named_formats_are_four_bytes_a_pixel() {
        assert_eq!(min_stride(FourCc::ARGB8888, 8), Some(32));
        assert_eq!(min_stride(FourCc::XRGB8888, 8), Some(32));
        assert_eq!(min_stride(FourCc::ABGR8888, 1), Some(4));
    }

    /// The sub-sampled entries are the only ones where the round-up is visible:
    /// three pixels of `YVYU` still need two blocks.
    #[test]
    fn a_sub_sampled_format_rounds_its_last_block_up() {
        let yvyu = FourCc::from_chars(b'Y', b'V', b'Y', b'U');
        assert_eq!(min_stride(yvyu, 2), Some(4));
        assert_eq!(min_stride(yvyu, 3), Some(6));
        assert_eq!(min_stride(yvyu, 4), Some(8));
    }

    /// An unlisted format is charged the widest block in the table, which is an
    /// upper bound over every listed one — so the fallback can never
    /// under-charge a format that is added to the table later.
    #[test]
    fn an_unknown_format_is_charged_the_widest_block() {
        let made_up = FourCc::from_chars(b'?', b'?', b'?', b'?');
        assert_eq!(min_stride(made_up, 4), None);
        assert_eq!(min_stride_bound(made_up, 4), 4 * MAX_BYTES_PER_BLOCK);

        for info in TABLE {
            assert!(
                info.bytes_per_block <= MAX_BYTES_PER_BLOCK,
                "{:?} needs more than the fallback charges",
                info.format
            );
        }
    }

    /// Every entry is distinct — a duplicate would mean a transcription slip,
    /// and `find` would silently answer with whichever came first.
    #[test]
    fn no_format_is_listed_twice() {
        for (i, a) in TABLE.iter().enumerate() {
            for b in &TABLE[i + 1..] {
                assert_ne!(a.format, b.format, "duplicate table entry");
            }
        }
    }
}
