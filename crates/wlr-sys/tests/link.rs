//! Version agreement between this crate and the headers bindgen read.
//!
//! # What is *not* checked on wlroots 0.17
//!
//! wlroots gained the `wlr_version_get_major/minor/micro` runtime accessors in
//! **0.20**. Before that the library exports no version symbol, so there is no
//! way to ask the linked `libwlroots.so` what it thinks it is — and therefore no
//! way to catch a build that read one wlroots' headers and linked another's.
//!
//! That gap is wider here than on 0.19. wlroots did not version-suffix its
//! pkg-config module until 0.19, so this branch probes a bare `wlroots.pc` and
//! the `range_version` check in `build.rs` is the only guard against picking up
//! the wrong series at all.
//!
//! `examples/headless.rs` remains the proxy: it fails to link if the symbols the
//! headers promised are absent from the library.

/// The versioning policy — "crate minor == wlroots minor" — enforced rather than
/// merely documented.
#[test]
fn crate_version_tracks_wlroots_version() {
    let crate_major: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap();
    let crate_minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap();

    assert_eq!(
        crate_major, 0,
        "wlr-sys is 0.x while wlroots is; revisit the versioning policy if that changed"
    );
    assert_eq!(
        crate_minor,
        wlr_sys::WLR_VERSION_MINOR,
        "crate minor ({crate_minor}) must equal the wlroots minor it binds ({}); \
         see docs/RELEASING.md",
        wlr_sys::WLR_VERSION_MINOR
    );
    assert_eq!(wlr_sys::WLR_VERSION_MAJOR, 0);
}

/// The headers bindgen read really were 0.17's.
///
/// This matters more on this branch than on 0.19+: with an unversioned
/// `wlroots.pc`, nothing in the module *name* pins the series.
#[test]
fn headers_are_wlroots_0_17() {
    assert_eq!(wlr_sys::WLR_VERSION_MAJOR, 0);
    assert_eq!(wlr_sys::WLR_VERSION_MINOR, 17);

    let version = std::str::from_utf8(wlr_sys::WLR_VERSION_STR)
        .expect("WLR_VERSION_STR is not UTF-8")
        .trim_end_matches('\0');
    assert!(
        version.starts_with("0.17."),
        "WLR_VERSION_STR is {version:?}, expected a 0.17.x release"
    );
}
