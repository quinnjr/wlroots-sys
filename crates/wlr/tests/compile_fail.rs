//! Compile-fail proofs backing this crate's central safety claim: a consumer
//! cannot mint an `Output` handle with a lifetime of their own choosing,
//! because its only constructor is `pub(crate)`.
//!
//! `from_raw_is_not_reachable_outside_the_crate` is the case that actually
//! proves that claim — it fails only because `Output::from_raw` is private,
//! and it would start compiling (and so fail as a test) the moment that
//! visibility were ever widened to `pub`. If that ever happens, handles can
//! be minted with an arbitrary lifetime and every guarantee in the crate is
//! void.
//!
//! `output_lifetime_parameter_and_calling_convention_are_pinned` is a weaker,
//! regression-only case. It locks two things: that a handle cannot be
//! stored past the borrow that produced it (the intended calling
//! convention), and that `Output` carries exactly one lifetime parameter
//! (removing it turns the fixture's error into E0107, "wrong number of
//! lifetime arguments," rather than leaving the fixture compiling). It does
//! **not** discriminate `Output`'s safety design from an arbitrary
//! `struct Foo<'h>` — the identical fixture, with the identical error,
//! would still fail to compile even if `from_raw` were `pub`. It is kept
//! because it documents real intended usage, not because it proves anything
//! about `Output`'s soundness.

#[test]
fn output_lifetime_parameter_and_calling_convention_are_pinned() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/output_escapes_handler.rs");
}

#[test]
fn from_raw_is_not_reachable_outside_the_crate() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/output_from_raw_is_private.rs");
}
