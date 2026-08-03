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
    let (major, minor) = unsafe {
        (
            wlr_sys::wlr_version_get_major(),
            wlr_sys::wlr_version_get_minor(),
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
}

#[test]
fn crate_targets_wlroots_0_20() {
    assert_eq!(wlr_sys::WLR_VERSION_MAJOR, 0);
    assert_eq!(wlr_sys::WLR_VERSION_MINOR, 20);
}
