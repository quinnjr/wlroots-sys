//! Proves that the library we linked against is the library whose headers
//! bindgen read.
//!
//! `WLR_VERSION_*` are `#define`s that bindgen lifted out of `wlr/version.h`;
//! `wlr_version_get_*` are functions compiled into `libwlroots-0.20.so`. If the
//! build picked up headers from one wlroots installation and a shared object from
//! another, these disagree — and every struct offset in the crate is suspect.

#[test]
fn linked_library_matches_headers() {
    // SAFETY: plain accessors, no arguments, no state.
    let (major, minor, micro) = unsafe {
        (
            wlr_sys::wlr_version_get_major(),
            wlr_sys::wlr_version_get_minor(),
            wlr_sys::wlr_version_get_micro(),
        )
    };

    assert_eq!(
        major as u32,
        wlr_sys::WLR_VERSION_MAJOR,
        "linked libwlroots major version does not match the headers bindgen read"
    );
    assert_eq!(
        minor as u32,
        wlr_sys::WLR_VERSION_MINOR,
        "linked libwlroots minor version does not match the headers bindgen read"
    );
    // wlroots offers no ABI guarantee across patch releases either, so a build
    // that read 0.20.2 headers and linked 0.20.1 has suspect struct offsets.
    assert_eq!(
        micro as u32,
        wlr_sys::WLR_VERSION_MICRO,
        "linked libwlroots patch version does not match the headers bindgen read"
    );
}

/// The versioning policy — "crate minor == wlroots minor" — enforced rather than
/// merely documented.
///
/// Comparing against `CARGO_PKG_VERSION_*` instead of a literal `20` means a
/// wlroots bump that forgets to move the crate version (or vice versa) fails
/// here, which is one fewer site in the release checklist that can silently rot.
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
