//! Compile-fail proofs backing this crate's central safety claim: a consumer
//! cannot mint a handle — `Output` or `Toplevel` — with a lifetime of their
//! own choosing, because the only constructor of each is `pub(crate)`.
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

/// The `Toplevel` half of the pair above, and for the same reasons: this is
/// the weaker, regression-only case, pinning the calling convention and the
/// single lifetime parameter.
#[test]
fn toplevel_lifetime_parameter_and_calling_convention_are_pinned() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/toplevel_escapes_handler.rs");
}

/// The case that actually proves a consumer cannot mint a `Toplevel`. It
/// would start compiling — and so fail as a test — the moment
/// `Toplevel::from_raw_with_id` were ever widened to `pub`.
#[test]
fn toplevel_from_raw_with_id_is_not_reachable_outside_the_crate() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/toplevel_from_raw_is_private.rs");
}

/// `RegionRef` is a handle by the same argument as `Output` and `Toplevel`: it
/// borrows a `pixman_region32` a wlroots object owns, and outliving that object
/// is a use-after-free. The weaker, regression-only half of the pair.
#[test]
fn region_ref_lifetime_parameter_and_calling_convention_are_pinned() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/region_ref_escapes_borrow.rs");
}

/// The load-bearing half: it would start compiling the moment
/// `RegionRef::from_raw` were widened to `pub`, which is what would let a
/// consumer mint a view with a lifetime of their own choosing.
#[test]
fn region_ref_from_raw_is_not_reachable_outside_the_crate() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/region_ref_from_raw_is_private.rs");
}

/// A texture outliving its renderer is a double free — the pixman renderer
/// destroys every texture it still knows about when it goes, and
/// `wlr_texture_destroy` afterwards frees the same allocation again. This is
/// the borrow that prevents it, and unlike the pairs above it is not merely a
/// calling convention: the ordering it pins is a memory-safety requirement of
/// wlroots' own API.
#[test]
fn a_texture_cannot_outlive_its_renderer() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/texture_outlives_renderer.rs");
}

/// The load-bearing half for textures: `Texture::from_raw` is `pub(crate)`, so
/// a consumer cannot mint one with a lifetime that escapes the check above.
#[test]
fn texture_from_raw_is_not_reachable_outside_the_crate() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/texture_from_raw_is_private.rs");
}

/// Dropping a render pass submits it, so a pass that outlived its renderer
/// would submit through freed memory. `RenderPass<'r, 'b>` borrows both the
/// renderer and the destination buffer; this pins that it cannot be returned
/// from the scope that made it.
#[test]
fn a_render_pass_cannot_escape_the_scope_that_began_it() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/pass_escapes_renderer.rs");
}

/// A `RenderTimer` a pass names has to outlive the pass — wlroots keeps the
/// pointer and writes through it from inside `submit`, so destroying the timer
/// first is a use-after-free on any renderer that implements timers.
/// `begin_buffer_pass` takes its options at the pass's own lifetime, which is
/// what makes that a compile error rather than a rule.
#[test]
fn a_render_timer_cannot_die_before_the_pass_that_names_it() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/timer_outlives_pass.rs");
}
