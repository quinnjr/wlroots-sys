//! Version agreement between this crate and the headers bindgen read.
//!
//! # What is *not* checked on wlroots 0.15
//!
//! wlroots gained `wlr_version_get_major/minor/micro` in **0.20**; before that
//! the library exports no version symbol, so there is no way to ask the linked
//! `libwlroots.so` what it thinks it is.
//!
//! That gap is widest here. wlroots did not version-suffix its pkg-config module
//! until 0.19, so this branch probes a bare `wlroots.pc` and the `range_version`
//! check in `build.rs` is the only guard against picking up the wrong series.
//!
//! `examples/headless.rs` remains the proxy: it fails to link if the symbols the
//! headers promised are absent from the library.

/// The versioning policy — "crate minor == wlroots minor" — enforced rather than
/// merely documented.
#[test]
fn crate_version_tracks_wlroots_version() {
    let crate_major: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap();
    let crate_minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap();

    assert_eq!(crate_major, 0);
    assert_eq!(
        crate_minor,
        wlr_sys::WLR_VERSION_MINOR,
        "crate minor ({crate_minor}) must equal the wlroots minor it binds ({}); \
         see docs/RELEASING.md",
        wlr_sys::WLR_VERSION_MINOR
    );
    assert_eq!(wlr_sys::WLR_VERSION_MAJOR, 0);
}

/// The headers bindgen read really were 0.15's.
#[test]
fn headers_are_wlroots_0_15() {
    assert_eq!(wlr_sys::WLR_VERSION_MAJOR, 0);
    assert_eq!(wlr_sys::WLR_VERSION_MINOR, 15);

    let version = std::str::from_utf8(wlr_sys::WLR_VERSION_STR)
        .expect("WLR_VERSION_STR is not UTF-8")
        .trim_end_matches('\0');
    assert!(
        version.starts_with("0.15."),
        "WLR_VERSION_STR is {version:?}, expected a 0.15.x release"
    );
}
