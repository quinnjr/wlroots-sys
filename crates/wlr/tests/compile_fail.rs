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
//!
//! # When these fail after a toolchain update
//!
//! The `.stderr` fixtures next to each case pin rustc's *exact* diagnostic
//! text, notes and spans included, and CI runs `cargo test --workspace` on
//! stable — which moves. So a rustc release that rewords a note or renumbers a
//! span breaks these tests without anything in this crate having changed, and
//! it will look like a `wlr` regression rather than what it is.
//!
//! Read the diff trybuild prints before doing anything. If the *error* is still
//! the same error — still E0603/"private", still the same borrow complaint —
//! it is a formatting change: regenerate with
//!
//! ```sh
//! TRYBUILD=overwrite cargo test -p wlr --test compile_fail
//! ```
//!
//! and commit the new `.stderr`. If the error code or the reason changed, the
//! guarantee changed with it; fix the crate, not the fixture.

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

/// Pins the unstated assumption `dispatch.rs`'s thread-local `IN_HANDLER`
/// guard rests on: `Display`, `EventLoop`, `Backend` and `Output` must all be
/// `!Send`, or a handler could move one to another thread and find the flag
/// clear there. See the fixture's own doc comment for the full argument.
#[test]
fn thread_scoped_types_are_not_send() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/thread_scoped_types_are_not_send.rs");
}
