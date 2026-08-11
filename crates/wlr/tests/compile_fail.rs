//! The crate's central safety claim, tested rather than asserted.
//!
//! `Output<'h>` is bound to the dispatch call that produced it. If this test
//! ever passes compilation, handles can escape handlers and every guarantee in
//! the crate is void.

#[test]
fn handles_cannot_escape_their_handler() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/output_escapes_handler.rs");
}

#[test]
fn from_raw_is_not_reachable_outside_the_crate() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/output_from_raw_is_private.rs");
}
