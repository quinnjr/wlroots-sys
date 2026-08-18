//! `render/color.h`: the colour vocabulary and colour transforms.
//!
//! Nothing here needs a backend, a GPU or a display — `wlr_color_transform` is
//! plain heap state and the primaries maths is pure — so every test runs
//! everywhere CI does.
//!
//! The pinning tests are the point of the file. wlroots' colour enums are a
//! mix of bitmask-valued and sequential ones, and which is which is invisible
//! from the Rust side: `ColorEncoding::Bt709` maps to 2 while
//! `ColorRange::Limited` maps to 1, and swapping the two conventions produces
//! code that compiles, runs, and quietly mis-tags every texture.

use wlr::{
    AlphaMode, ChromaLocation, Cie1931Xy, ColorEncoding, ColorEncodings, ColorLuminances,
    ColorPrimaries, ColorRange, ColorTransform, Error, NamedPrimaries, TransferFunction,
    TransferFunctions,
};

/// How close two floats have to be for these tests. The primaries are given to
/// four decimal places in H.273 and stored as `f32`.
const EPS: f32 = 1e-4;

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= EPS
}

/// `wlr_color_named_primaries` is a **bitmask**: `SRGB = 1 << 0`,
/// `BT2020 = 1 << 1`. A sequential reading would put BT2020 at 1, which is
/// sRGB's value — the two would silently swap.
#[test]
fn named_primaries_are_bitmask_valued() {
    assert_eq!(
        wlr_sys::wlr_color_named_primaries::from(NamedPrimaries::Srgb),
        wlr_sys::wlr_color_named_primaries::WLR_COLOR_NAMED_PRIMARIES_SRGB
    );
    assert_eq!(
        wlr_sys::wlr_color_named_primaries::from(NamedPrimaries::Bt2020),
        wlr_sys::wlr_color_named_primaries::WLR_COLOR_NAMED_PRIMARIES_BT2020
    );
    assert_eq!(
        wlr_sys::wlr_color_named_primaries::from(NamedPrimaries::Srgb).0,
        1 << 0
    );
    assert_eq!(
        wlr_sys::wlr_color_named_primaries::from(NamedPrimaries::Bt2020).0,
        1 << 1
    );
}

/// `wlr_color_transfer_function` is a **bitmask with no zero variant** — the
/// trap this whole module is arranged around. The pass options carry it as an
/// `Option` precisely because 0 is not a member.
#[test]
fn transfer_functions_are_bitmask_valued_and_have_no_zero() {
    use wlr_sys::wlr_color_transfer_function as C;

    let pairs = [
        (
            TransferFunction::Srgb,
            C::WLR_COLOR_TRANSFER_FUNCTION_SRGB,
            1 << 0,
        ),
        (
            TransferFunction::St2084Pq,
            C::WLR_COLOR_TRANSFER_FUNCTION_ST2084_PQ,
            1 << 1,
        ),
        (
            TransferFunction::ExtLinear,
            C::WLR_COLOR_TRANSFER_FUNCTION_EXT_LINEAR,
            1 << 2,
        ),
        (
            TransferFunction::Gamma22,
            C::WLR_COLOR_TRANSFER_FUNCTION_GAMMA22,
            1 << 3,
        ),
        (
            TransferFunction::Bt1886,
            C::WLR_COLOR_TRANSFER_FUNCTION_BT1886,
            1 << 4,
        ),
    ];
    for (rust, c, bit) in pairs {
        assert_eq!(C::from(rust), c, "{rust:?}");
        assert_eq!(C::from(rust).0, bit, "{rust:?}");
        assert_ne!(C::from(rust).0, 0, "no transfer function is the zero value");
    }
}

/// `wlr_color_encoding` is a bitmask whose zero *is* a variant: `NONE = 0`,
/// then `IDENTITY = 1 << 0`. So `Bt709` is 2, not 2-as-an-ordinal.
#[test]
fn color_encodings_are_bitmask_valued_above_a_zero_none() {
    use wlr_sys::wlr_color_encoding as C;

    let pairs = [
        (ColorEncoding::None, C::WLR_COLOR_ENCODING_NONE, 0),
        (
            ColorEncoding::Identity,
            C::WLR_COLOR_ENCODING_IDENTITY,
            1 << 0,
        ),
        (ColorEncoding::Bt709, C::WLR_COLOR_ENCODING_BT709, 1 << 1),
        (ColorEncoding::Fcc, C::WLR_COLOR_ENCODING_FCC, 1 << 2),
        (ColorEncoding::Bt601, C::WLR_COLOR_ENCODING_BT601, 1 << 3),
        (
            ColorEncoding::Smpte240,
            C::WLR_COLOR_ENCODING_SMPTE240,
            1 << 4,
        ),
        (ColorEncoding::Bt2020, C::WLR_COLOR_ENCODING_BT2020, 1 << 5),
        (
            ColorEncoding::Bt2020Cl,
            C::WLR_COLOR_ENCODING_BT2020_CL,
            1 << 6,
        ),
        (ColorEncoding::Ictcp, C::WLR_COLOR_ENCODING_ICTCP, 1 << 7),
    ];
    for (rust, c, bit) in pairs {
        assert_eq!(C::from(rust), c, "{rust:?}");
        assert_eq!(C::from(rust).0, bit, "{rust:?}");
    }
    assert_eq!(ColorEncoding::default(), ColorEncoding::None);
}

/// `wlr_color_range` and `wlr_color_chroma_location` are **sequential**, unlike
/// everything above. Reading them as bitmasks would put `Full` at 4.
#[test]
fn ranges_and_chroma_locations_are_sequential() {
    use wlr_sys::wlr_color_chroma_location as L;
    use wlr_sys::wlr_color_range as R;

    assert_eq!(R::from(ColorRange::None).0, 0);
    assert_eq!(R::from(ColorRange::Limited).0, 1);
    assert_eq!(R::from(ColorRange::Full).0, 2);
    assert_eq!(R::from(ColorRange::Full), R::WLR_COLOR_RANGE_FULL);
    assert_eq!(ColorRange::default(), ColorRange::None);

    let chroma = [
        ChromaLocation::None,
        ChromaLocation::Type0,
        ChromaLocation::Type1,
        ChromaLocation::Type2,
        ChromaLocation::Type3,
        ChromaLocation::Type4,
        ChromaLocation::Type5,
    ];
    for (i, c) in chroma.into_iter().enumerate() {
        assert_eq!(L::from(c).0, i as u32, "{c:?}");
    }
    assert_eq!(
        L::from(ChromaLocation::Type5),
        L::WLR_COLOR_CHROMA_LOCATION_TYPE5
    );
    assert_eq!(ChromaLocation::default(), ChromaLocation::None);
}

/// `wlr_alpha_mode` has no "unset": zero is premultiplied-electrical, which is
/// a real mode. A `Default` that meant "unknown" would be a lie about the
/// zeroed struct.
#[test]
fn alpha_modes_are_sequential_and_zero_is_a_real_mode() {
    use wlr_sys::wlr_alpha_mode as A;

    assert_eq!(A::from(AlphaMode::PremultipliedElectrical).0, 0);
    assert_eq!(A::from(AlphaMode::PremultipliedOptical).0, 1);
    assert_eq!(A::from(AlphaMode::Straight).0, 2);
    assert_eq!(
        A::from(AlphaMode::default()),
        A::WLR_COLOR_ALPHA_MODE_PREMULTIPLIED_ELECTRICAL
    );
}

/// The set types are the other half of the bitmask/single-value split: a mask
/// holds several encodings at once, which the single-valued enum cannot.
#[test]
fn the_set_types_accumulate_bits_the_single_valued_enums_cannot() {
    let set = ColorEncodings::NONE
        .with(ColorEncoding::Bt709)
        .with(ColorEncoding::Bt2020);
    assert!(set.contains(ColorEncoding::Bt709));
    assert!(set.contains(ColorEncoding::Bt2020));
    assert!(!set.contains(ColorEncoding::Bt601));
    assert_eq!(set.bits(), (1 << 1) | (1 << 5));
    assert!(!set.is_empty());
    assert!(ColorEncodings::NONE.is_empty());
    assert_eq!(ColorEncodings::from_bits(set.bits()), set);

    let tfs = TransferFunctions::NONE
        .with(TransferFunction::Srgb)
        .with(TransferFunction::Bt1886);
    assert!(tfs.contains(TransferFunction::Srgb));
    assert!(!tfs.contains(TransferFunction::Gamma22));
    assert_eq!(tfs.bits(), (1 << 0) | (1 << 4));
    assert!(TransferFunctions::NONE.is_empty());
}

/// The sRGB chromaticities are H.273 code point 1 and are not something this
/// crate may round or restate — they come out of wlroots verbatim.
#[test]
fn named_srgb_primaries_are_the_h273_values() {
    let srgb = ColorPrimaries::named(NamedPrimaries::Srgb);
    assert!(
        close(srgb.red.x, 0.640) && close(srgb.red.y, 0.330),
        "{srgb:?}"
    );
    assert!(
        close(srgb.green.x, 0.300) && close(srgb.green.y, 0.600),
        "{srgb:?}"
    );
    assert!(
        close(srgb.blue.x, 0.150) && close(srgb.blue.y, 0.060),
        "{srgb:?}"
    );
    assert!(
        close(srgb.white.x, 0.3127) && close(srgb.white.y, 0.3290),
        "{srgb:?}"
    );

    let bt2020 = ColorPrimaries::named(NamedPrimaries::Bt2020);
    assert!(
        close(bt2020.red.x, 0.708) && close(bt2020.red.y, 0.292),
        "{bt2020:?}"
    );
    assert_ne!(srgb, bt2020, "two different colour volumes");
    // The white point is D65 in both, which is what makes a conversion between
    // them a pure primaries change.
    assert_eq!(srgb.white, bt2020.white);
}

/// Converting a colour volume to itself has to be the identity, or every
/// colour-managed draw that happens not to change volume would shift.
#[test]
fn converting_a_colour_volume_to_itself_is_the_identity_matrix() {
    let srgb = ColorPrimaries::named(NamedPrimaries::Srgb);
    let m = srgb
        .transform_absolute_colorimetric(&srgb)
        .expect("sRGB is not degenerate");
    #[rustfmt::skip]
    let identity = [
        1.0f32, 0.0, 0.0,
        0.0, 1.0, 0.0,
        0.0, 0.0, 1.0,
    ];
    for (i, (got, want)) in m.iter().zip(identity).enumerate() {
        assert!(close(*got, want), "element {i} of {m:?}");
    }
}

/// sRGB → BT.2020 must not be the identity, which is what proves the test
/// above is checking something rather than reading zeroes back.
#[test]
fn converting_between_two_volumes_is_not_the_identity() {
    let srgb = ColorPrimaries::named(NamedPrimaries::Srgb);
    let bt2020 = ColorPrimaries::named(NamedPrimaries::Bt2020);
    let m = srgb
        .transform_absolute_colorimetric(&bt2020)
        .expect("neither volume is degenerate");
    assert!(
        !close(m[0], 1.0) || !close(m[1], 0.0),
        "sRGB and BT.2020 have different primaries: {m:?}"
    );
    // A conversion between two volumes with the same white point maps white to
    // white, so every row sums to 1.
    for row in 0..3 {
        let sum: f32 = m[row * 3..row * 3 + 3].iter().sum();
        assert!(close(sum, 1.0), "row {row} of {m:?} sums to {sum}");
    }
}

/// `ColorPrimaries` has public `f32` fields and a `Default`, so a degenerate
/// colour volume is one struct literal away — and wlroots inverts it with
/// `matrix_invert`, whose `assert(det != 0)` aborts the whole compositor in the
/// distro build.
///
/// `ColorPrimaries::default()` **did** abort the test binary before the check
/// existed; that is the case this test exists for. The rest are singular only
/// up to rounding, so wlroots answers them with a matrix of unusable
/// magnitudes rather than an abort — refused here for the same reason.
#[test]
fn a_degenerate_colour_volume_is_refused_instead_of_aborting() {
    let srgb = ColorPrimaries::named(NamedPrimaries::Srgb);
    let refused = Error::Operation("ColorPrimaries::transform_absolute_colorimetric");

    // All zeroes: every chromaticity has `y == 0`, so wlroots' `xy_to_xyz`
    // answers the zero vector and the matrix is entirely zero.
    let zeroed = ColorPrimaries::default();
    assert_eq!(
        zeroed.transform_absolute_colorimetric(&zeroed).unwrap_err(),
        refused
    );
    // Degenerate on either side alone, not just both.
    assert_eq!(
        srgb.transform_absolute_colorimetric(&zeroed).unwrap_err(),
        refused
    );
    assert_eq!(
        zeroed.transform_absolute_colorimetric(&srgb).unwrap_err(),
        refused
    );

    // Two primaries at the same chromaticity: the RGB→XYZ matrix has two equal
    // columns, so it is singular without any zero in it.
    let collinear = ColorPrimaries {
        red: Cie1931Xy { x: 0.64, y: 0.33 },
        green: Cie1931Xy { x: 0.64, y: 0.33 },
        blue: Cie1931Xy { x: 0.15, y: 0.06 },
        white: Cie1931Xy {
            x: 0.3127,
            y: 0.3290,
        },
    };
    assert_eq!(
        collinear
            .transform_absolute_colorimetric(&srgb)
            .unwrap_err(),
        refused
    );

    // A white point sitting exactly on the red primary: the RGB→XYZ matrix is
    // fine, but the scaling vector has two zero components, which is the
    // *second* inversion wlroots does and the one only the destination
    // reaches.
    let white_on_red = ColorPrimaries {
        white: srgb.red,
        ..srgb
    };
    assert_eq!(
        srgb.transform_absolute_colorimetric(&white_on_red)
            .unwrap_err(),
        refused
    );

    // And the real volumes still work, so the check is not simply refusing
    // everything.
    assert!(srgb.transform_absolute_colorimetric(&srgb).is_ok());
    assert!(
        white_on_red
            .transform_absolute_colorimetric(&srgb)
            .is_ok_and(|m| m.iter().all(|v| v.is_finite())),
        "a degenerate *source* only reaches the first inversion, which succeeds"
    );
}

/// `transform_absolute_colorimetric` produces a matrix in exactly the order
/// `ColorTransform::matrix` consumes — row-major — so the two compose. If they
/// disagreed, a colour-managed compositor would transpose every conversion.
#[test]
fn the_primaries_matrix_feeds_straight_into_a_matrix_transform() {
    let srgb = ColorPrimaries::named(NamedPrimaries::Srgb);
    let identity = srgb
        .transform_absolute_colorimetric(&srgb)
        .expect("sRGB is not degenerate");
    let tr = ColorTransform::matrix(identity).expect("matrix transform");
    let out = tr.eval([0.1, 0.2, 0.3]);
    for (got, want) in out.iter().zip([0.1f32, 0.2, 0.3]) {
        assert!(close(*got, want), "{out:?}");
    }
}

/// The matrix constructor is row-major, verified against
/// `multiply_matrix_vector` in wlroots' `render/color.c` rather than guessed:
/// a matrix that is not its own transpose answers differently under the two
/// conventions.
#[test]
fn a_matrix_transform_is_row_major() {
    #[rustfmt::skip]
    let rotate = [
        0.0f32, 1.0, 0.0,
        0.0, 0.0, 1.0,
        1.0, 0.0, 0.0,
    ];
    let tr = ColorTransform::matrix(rotate).expect("matrix transform");
    // Row-major gives [g, b, r]; column-major would give [b, r, g].
    assert_eq!(tr.eval([1.0, 2.0, 3.0]), [2.0, 3.0, 1.0]);
}

/// A pipeline applies its inputs left to right, and the composition has to be
/// observable — two transforms whose order matters.
#[test]
fn a_pipeline_composes_its_inputs_in_order() {
    #[rustfmt::skip]
    let swap_rg = ColorTransform::matrix([
        0.0, 1.0, 0.0,
        1.0, 0.0, 0.0,
        0.0, 0.0, 1.0,
    ]).expect("matrix transform");
    #[rustfmt::skip]
    let swap_gb = ColorTransform::matrix([
        1.0, 0.0, 0.0,
        0.0, 0.0, 1.0,
        0.0, 1.0, 0.0,
    ]).expect("matrix transform");

    let rg_then_gb =
        ColorTransform::pipeline(&[swap_rg.clone(), swap_gb.clone()]).expect("pipeline");
    let gb_then_rg = ColorTransform::pipeline(&[swap_gb, swap_rg]).expect("pipeline");

    // (r, g, b) = (1, 2, 3): swap r/g gives (2, 1, 3), then swap g/b gives
    // (2, 3, 1). The other order gives (1, 3, 2) then (3, 1, 2).
    assert_eq!(rg_then_gb.eval([1.0, 2.0, 3.0]), [2.0, 3.0, 1.0]);
    assert_eq!(gb_then_rg.eval([1.0, 2.0, 3.0]), [3.0, 1.0, 2.0]);
}

/// A pipeline takes its own reference on every input
/// (`wlr_color_transform_init_pipeline` in `render/color.c`), so dropping the
/// inputs afterwards must leave it usable. Getting this wrong is a
/// use-after-free that only shows up once the allocator reuses the block.
#[test]
fn a_pipeline_keeps_its_inputs_alive_after_the_caller_drops_them() {
    let pipeline = {
        #[rustfmt::skip]
        let a = ColorTransform::matrix([
            2.0, 0.0, 0.0,
            0.0, 1.0, 0.0,
            0.0, 0.0, 1.0,
        ]).expect("matrix transform");
        #[rustfmt::skip]
        let b = ColorTransform::matrix([
            1.0, 0.0, 0.0,
            0.0, 3.0, 0.0,
            0.0, 0.0, 1.0,
        ]).expect("matrix transform");
        ColorTransform::pipeline(&[a, b]).expect("pipeline")
        // `a` and `b` are dropped here.
    };
    assert_eq!(pipeline.eval([1.0, 1.0, 1.0]), [2.0, 3.0, 1.0]);
}

/// `Clone` is a reference, not a copy, and the count has to balance. A leak
/// would pass this; a double unref would abort on the assertion wlroots
/// compiles in, which is what makes running it worthwhile.
#[test]
fn cloning_and_dropping_a_transform_a_thousand_times_balances() {
    let tr = ColorTransform::inverse_eotf(TransferFunction::Srgb).expect("inverse eotf");
    let clones: Vec<ColorTransform> = (0..1000).map(|_| tr.clone()).collect();
    // Every clone names the same object: the type is immutable, so wlroots
    // hands the same pointer back.
    for clone in &clones {
        assert_eq!(clone.as_ptr(), tr.as_ptr());
    }
    drop(clones);
    // Still alive, and still the transform it was.
    assert!(
        tr.eval([0.5, 0.5, 0.5])[0] > 0.5,
        "sRGB EOTF⁻¹ lifts midtones"
    );
}

/// A lookup table has to be interpolated the way wlroots says: `dim` entries
/// spanning 0..=1, linearly between them.
#[test]
fn a_lookup_table_transform_maps_through_its_entries() {
    // Two entries: 0 at input 0, full scale at input 1. That is the identity.
    let ramp = [0u16, u16::MAX];
    let tr = ColorTransform::lut_3x1d(&ramp, &ramp, &ramp).expect("lut");
    let out = tr.eval([0.0, 0.5, 1.0]);
    assert!(
        close(out[0], 0.0) && close(out[1], 0.5) && close(out[2], 1.0),
        "{out:?}"
    );

    // An inverted ramp on the red channel only.
    let inverted = [u16::MAX, 0u16];
    let tr = ColorTransform::lut_3x1d(&inverted, &ramp, &ramp).expect("lut");
    let out = tr.eval([0.25, 0.25, 0.25]);
    assert!(close(out[0], 0.75), "{out:?}");
    assert!(close(out[1], 0.25), "{out:?}");
}

/// Mismatched or empty tables are refused in Rust: wlroots reads `dim` entries
/// from all three pointers whatever their real lengths, and a `dim` of 0 makes
/// its evaluator index at `SIZE_MAX`.
#[test]
fn a_malformed_lookup_table_never_reaches_wlroots() {
    let four = [0u16, 1, 2, 3];
    let two = [0u16, 1];
    assert_eq!(
        ColorTransform::lut_3x1d(&four, &two, &four).unwrap_err(),
        Error::Operation("ColorTransform::lut_3x1d")
    );
    assert_eq!(
        ColorTransform::lut_3x1d(&four, &four, &two).unwrap_err(),
        Error::Operation("ColorTransform::lut_3x1d")
    );
    assert_eq!(
        ColorTransform::lut_3x1d(&[], &[], &[]).unwrap_err(),
        Error::Operation("ColorTransform::lut_3x1d")
    );
}

/// `wlr_color_transform_init_pipeline` **asserts** a non-empty input array, and
/// Arch's wlroots is built without `NDEBUG`, so the assertion aborts the
/// process. This test passing is the evidence that it cannot be reached.
#[test]
fn an_empty_pipeline_is_refused_instead_of_aborting() {
    assert_eq!(
        ColorTransform::pipeline(&[]).unwrap_err(),
        Error::Operation("ColorTransform::pipeline")
    );
}

/// The three plain value types are `#[repr(C)]` twins of wlroots' structs and
/// are checked against them at compile time; this pins that they are also
/// ordinary values a consumer can build, compare and default.
#[test]
fn the_colour_value_types_are_plain_data() {
    let xy = Cie1931Xy { x: 0.5, y: 0.25 };
    assert_eq!(xy, Cie1931Xy { x: 0.5, y: 0.25 });
    assert_eq!(Cie1931Xy::default(), Cie1931Xy { x: 0.0, y: 0.0 });

    let lum = ColorLuminances {
        min: 0.005,
        max: 10_000.0,
        reference: 203.0,
    };
    assert_eq!(lum.reference, 203.0);
    assert_eq!(ColorLuminances::default().max, 0.0);

    let primaries = ColorPrimaries {
        red: xy,
        ..ColorPrimaries::default()
    };
    assert_eq!(primaries.red, xy);
    assert_eq!(primaries.blue, Cie1931Xy::default());
}
