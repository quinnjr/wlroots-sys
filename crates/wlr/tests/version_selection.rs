//! This branch's `wlr` binds this branch's wlroots, and says so at runtime.
//!
//! Version selection is a branch, not a feature: cargo cannot resolve a
//! manifest listing two `wlr-sys` minors, because its `links` uniqueness check
//! runs across every dependency edge rather than the activated feature set.
//! What remains to test is that the `wlr-sys` we linked really is the minor
//! this branch claims — a mismatched path dependency or a stale lockfile would
//! otherwise go unnoticed until a symbol failed to resolve.

#[test]
fn linked_wlroots_is_this_branchs_minor() {
    let (major, minor) = wlr::wlroots_version();

    assert_eq!(major, 0, "wlroots is 0.x");
    assert_eq!(
        minor, 19,
        "this branch binds wlroots 0.19; a different minor means the wlr-sys \
         dependency does not match the branch"
    );
}
