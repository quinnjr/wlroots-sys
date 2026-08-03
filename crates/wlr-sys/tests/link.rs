//! Version agreement between this crate, the headers bindgen read, and the
//! library we linked.
//!
//! # What is *not* checked on wlroots 0.19
//!
//! wlroots gained the `wlr_version_get_major/minor/micro` runtime accessors in
//! **0.20**. Before that the library exports no version symbol at all, so there
//! is no way to ask the linked `libwlroots-0.19.so` what it thinks it is — and
//! therefore no way to catch a build that read 0.19.2's headers and linked
//! 0.19.1's shared object. wlroots offers no ABI guarantee across patch releases
//! either, so that mismatch would leave every struct offset suspect.
//!
//! The `wlr-sys 0.20.x` series asserts this properly. Here the best available
//! proxy is `examples/headless.rs`, which fails to link if the symbols the
//! headers promised are not in the library at all.

/// The versioning policy — "crate minor == wlroots minor" — enforced rather than
/// merely documented.
///
/// Comparing against `CARGO_PKG_VERSION_*` instead of a literal means a wlroots
/// bump that forgets to move the crate version (or vice versa) fails here, which
/// is one fewer site in the release checklist that can silently rot.
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

/// The headers bindgen read really were 0.19's.
///
/// Cheap, but it is what stands between "pkg-config resolved something" and
/// "pkg-config resolved the wlroots we meant" — the range check in `build.rs`
/// runs against the `.pc` file, this runs against the headers themselves.
#[test]
fn headers_are_wlroots_0_19() {
    assert_eq!(wlr_sys::WLR_VERSION_MAJOR, 0);
    assert_eq!(wlr_sys::WLR_VERSION_MINOR, 19);

    let version = std::str::from_utf8(wlr_sys::WLR_VERSION_STR)
        .expect("WLR_VERSION_STR is not UTF-8")
        .trim_end_matches('\0');
    assert!(
        version.starts_with("0.19."),
        "WLR_VERSION_STR is {version:?}, expected a 0.19.x release"
    );
}
