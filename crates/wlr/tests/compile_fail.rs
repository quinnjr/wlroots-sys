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

/// A backend view — `Gles2`, `Vk`, `Pixman` — carries the proof that a
/// renderer is of that kind, and every accessor on it dereferences that
/// renderer. The borrow is what stops the proof outliving the thing it is
/// about.
///
/// GLES2 rather than pixman because the gated modules are the ones a build
/// script mistake could silently delete; if `wlr_has_gles2_renderer` ever
/// stopped being set, this fixture would fail to compile *for the wrong
/// reason*, which the `#[cfg]` on the test makes visible as a skipped test
/// rather than a passing one.
#[cfg(wlr_has_gles2_renderer)]
#[test]
fn a_backend_view_cannot_outlive_the_renderer_it_proves_something_about() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/gles2_view_outlives_renderer.rs");
}

/// A `SyncWaiter`'s destructor removes an event source from the display's
/// loop, so outliving the display is a use-after-free rather than a leak. The
/// borrow that prevents it is not in the plan's original sketch — it was added
/// once the destructor's requirements were pinned down — and this is what
/// keeps it.
#[test]
fn a_timeline_waiter_cannot_outlive_the_display_whose_loop_it_is_on() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/sync_waiter_outlives_display.rs");
}

/// The case that actually proves a consumer cannot mint a `SceneNode`. A node
/// is freed by a destroy cascade nobody announces, so a handle minted with a
/// lifetime of the consumer's own choosing is the sharpest use-after-free this
/// crate has available. It would start compiling — and so fail as a test — the
/// moment `SceneNode::from_raw_with_id` were widened to `pub`.
#[test]
fn scene_node_from_raw_with_id_is_not_reachable_outside_the_crate() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/scene_node_from_raw_is_private.rs");
}

/// The `SceneTree` half of the pair above: the tree handle has its own private
/// constructor, and the argument applies to it verbatim.
#[test]
fn scene_tree_from_raw_with_id_is_not_reachable_outside_the_crate() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/scene_tree_from_raw_is_private.rs");
}

/// The weaker, regression-only half for `SceneNode`: it pins the intended
/// calling convention — `Runtime::with_node` hands the handle out for the
/// duration of one closure and no longer — and that `SceneNode` carries
/// exactly one lifetime parameter.
#[test]
fn scene_node_lifetime_parameter_and_calling_convention_are_pinned() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/scene_node_escapes_borrow.rs");
}
