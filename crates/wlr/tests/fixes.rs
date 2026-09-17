//! The `wlr_fixes` global.
//!
//! Split out of `tests/security_context.rs`, where it lived as
//! `fixes_global_creates_once` next to unrelated manager tests: a global that
//! creates once belongs in a `fixes`-named home.

mod common;

use wlr::{Display, Error, Runtime};

/// The `wlr_fixes` global creates once and refuses a second create.
///
/// The refusal is [`Error::Operation`], not [`Error::Create`]: the crate
/// rejects the second create before calling C. The `Create`
/// (wlroots-returned-null) arm stays OOM-only — a failed allocation inside
/// wlroots, which no test can induce on purpose.
#[test]
fn fixes_global_creates_once() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    runtime.create_fixes(&display, 1).expect("fixes global");
    assert!(
        matches!(runtime.create_fixes(&display, 1), Err(Error::Operation(_))),
        "a second fixes global is refused before C is called"
    );
}
