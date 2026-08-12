//! wlr-layer-shell-unstable-v1: panels, launchers, wallpapers and other
//! surfaces anchored to an output's edges rather than positioned by the
//! compositor's own window management, added to this crate in 0.20.11.
//!
//! # The timing hazard: confirmed, and it is a hard abort
//!
//! xdg-decoration's `set_decoration_mode` (see `decoration.rs`'s own doc)
//! stages its answer instead of sending it immediately because
//! `wlr_xdg_surface_schedule_configure` asserts `surface->initialized`, and
//! that flag only flips true during the toplevel's first commit — so a
//! preference the client stated before ever committing (the ordinary case)
//! cannot be answered synchronously. `wlr_layer_surface_v1` carries the
//! identically-named fields `initialized`/`initial_commit`, and the
//! identical hazard is confirmed here too, from the shipped binary rather
//! than from source:
//!
//! `wlr_layer_surface_v1_configure` contains `assert(surface->initialized);`
//! at `types/wlr_layer_shell_v1.c:318` — confirmed by disassembling
//! `libwlroots-0.20.so` (the distribution ships wlroots **without**
//! `NDEBUG`, so the `__assert_fail` call and its four string arguments —
//! `"surface->initialized"`, `"types/wlr_layer_shell_v1.c"`, line `318`,
//! `"wlr_layer_surface_v1_configure"` — are present in the release binary)
//! and cross-checked against `offsetof(struct wlr_layer_surface_v1,
//! initialized)` computed from the installed 0.20 header, which matches the
//! offset the assert reads. **Calling `wlr_layer_surface_v1_configure`
//! before this surface's first commit aborts the whole compositor
//! process** — identical severity to the xdg-decoration hazard this
//! module's staged-answer shape is modeled on.
//!
//! It is load-bearing a second way, also confirmed from the binary:
//! `layer_surface_reset`, invoked from `layer_surface_role_commit` on the
//! surface's *unmap* commit, resets `initialized` back to `false` (proved by
//! the `assert(!surface->initialized)` immediately after the reset call in
//! the same disassembly). So a layer surface re-enters the uninitialized
//! state after **every** unmap, not only before its first ever commit, and
//! [`crate::Runtime::configure_layer_surface`] called from
//! [`crate::ToplevelHandler::layer_surface_unmapped`] — or from any point
//! after an unmap and before the surface's next commit — would abort
//! exactly the same way without this staging. The implementation already
//! handles this correctly, because it stages on wlroots' own `initialized`
//! flag rather than on any state of its own, and that flag mirrors both
//! windows (pre-first-commit and post-unmap) identically. **This is not
//! incidental and the staging must never be simplified away to an
//! unconditional immediate send** — that would reintroduce a
//! compositor-killing abort on two different, both entirely legal, client
//! orderings.
//!
//! [`crate::Runtime::configure_layer_surface`] applies the identical
//! staged-answer shape [`crate::decoration::DecorationEntry`] uses for
//! `set_decoration_mode`: a call that lands before the surface is initialized
//! records the size instead of sending it, and `backend.rs`'s
//! `on_layer_surface_commit` flushes it — for real, now that `initialized` is
//! true — the moment this surface's own initial commit is processed. See that
//! function's own doc for why a flush cannot double-send even when a handler
//! also answers from inside the commit callback itself.
//!
//! Unlike `DecorationEntry`, there is no `answered`/latching mechanism here:
//! xdg-decoration needs one because *two* independent sites
//! (`on_surface_commit`'s "the client never asked" path and
//! `on_new_toplevel_decoration`'s late-creation path) can each try to supply
//! a synthetic default and must not double-answer a request the other, or the
//! handler itself, already settled. A layer surface's configure has exactly
//! one origin — whatever [`crate::Runtime::configure_layer_surface`] itself
//! records — so there is nothing else that could race it, and reusing a
//! `Cell<Option<(u32, u32)>>` as its own "is anything outstanding" flag is
//! sufficient.
//!
//! # Answering `new_layer_surface` is mandatory
//!
//! Unlike xdg-shell, nothing in this crate's dispatch layer sends a
//! fallback configure for a layer surface. `on_surface_commit` unconditionally
//! schedules one for a toplevel that never got an answer; no equivalent
//! exists here, because there is no universally sane default size to invent
//! for a surface that asked for `0x0`. A [`crate::ToplevelHandler`] that
//! ignores [`crate::ToplevelHandler::new_layer_surface`] (and every
//! subsequent [`crate::ToplevelHandler::layer_surface_commit`]) therefore
//! leaves that client's layer surface **permanently unmapped** — it is
//! waiting for a configure that will never come. Call
//! [`crate::Runtime::configure_layer_surface`] from at least one of those two
//! handler methods for every layer surface this crate hands you, or that
//! client hangs forever.
//!
//! # `pending` vs `current`, and when each accessor reads which
//!
//! Every accessor on [`LayerSurface`] except
//! [`keyboard_interactive`](LayerSurface::keyboard_interactive) reads
//! `pending`, deliberately — see [`LayerSurface::layer`]'s own doc for why
//! that is right at `new_layer_surface`, before any commit exists.
//! `pending` and `current` agree whenever
//! [`crate::ToplevelHandler::layer_surface_commit`] is delivered
//! synchronously with the commit that produced it, which is the ordinary
//! case. They can disagree, narrowly, under deferred event delivery: if a
//! handler queues delivery rather than running it inline (see
//! `dispatch.rs`), further client requests may land in `pending` before the
//! queued handler call actually runs, so a `pending`-reading accessor can
//! report state the client has not committed yet by the time the handler
//! sees it. Worth knowing since these are frozen accessor semantics, even
//! though no handler in this crate's own examples triggers it.
//! [`keyboard_interactive`](LayerSurface::keyboard_interactive) reads
//! `current` instead, and has its own timing caveat — see its doc.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::id::find_id;
use crate::{OutputId, sys};

/// Identifies a layer surface for as long as the consumer chooses to
/// remember it.
///
/// Storable, comparable and hashable — unlike a handle, which cannot escape
/// the handler it was passed to. Ids are never reused within a process; see
/// [`crate::ToplevelId`], whose own doc this mirrors, for the full argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayerSurfaceId(pub(crate) u64);

impl LayerSurfaceId {
    /// An id no live layer surface can have, for testing the "unknown id"
    /// path.
    ///
    /// Public because "every by-id operation reports a miss rather than
    /// dereferencing" is a promise to consumers, and a promise nobody can
    /// write a test for is not one. Not for production code — an id from a
    /// real layer surface is the one [`LayerSurface::id`] returns, and it
    /// stops resolving once the [`Backend::run_all`](crate::Backend::run_all)
    /// call that announced it has returned, at which point it behaves
    /// exactly like this one.
    ///
    /// That every by-id operation misses on this value is part of the frozen
    /// contract, not an implementation accident: ids come from a process-wide
    /// counter that starts at 1, only ever increments, and never reuses a
    /// value, so `u64::MAX` cannot be handed to a real layer surface.
    pub fn dangling_for_test() -> LayerSurfaceId {
        LayerSurfaceId(u64::MAX)
    }
}

/// Which of wlr-layer-shell's four stacking bands a surface belongs to,
/// lowest to highest: `Background`, `Bottom`, `Top`, `Overlay`.
///
/// # The banded-tree scene design
///
/// This crate's scene graph gives each of the four layers its own scene
/// sub-tree, plus a fifth for ordinary toplevels, as direct children of the
/// scene root, created once in this fixed bottom-to-top order at
/// [`crate::Runtime::init_graphics`] time:
/// `Background` < `Bottom` < *toplevels* < `Top` < `Overlay`. See
/// `runtime.rs`'s `Graphics::background_band` for the mechanism (why
/// creating them in this order at start-of-day fixes their relative
/// stacking order permanently, using `wlr_scene_tree_create`'s own
/// append-at-the-end behavior).
///
/// A layer surface's scene node is created directly inside its own band
/// (`backend.rs`'s `on_new_layer_surface`), and reparented into a different
/// band (`wlr_scene_node_reparent`) whenever a later commit reports a
/// different `Layer` than the one it was placed under — a client is free to
/// send `set_layer` after mapping, and this crate follows that change
/// automatically rather than leaving the surface stacked where it started.
/// Every toplevel lives inside the toplevel band instead of directly under
/// the scene root; [`crate::Runtime::raise_toplevel`] now only reorders a
/// toplevel among *other toplevels*, which is correct because the bands
/// make the cross-band ordering structural rather than something any raise
/// call needs to maintain — see that method's own doc for the detail.
///
/// This is the design 0.20.11 ships with. It replaces a two-band
/// approximation that existed only on unpublished, pre-freeze commits of
/// this crate (collapsing all four layers into "below toplevels" / "above
/// toplevels" by raising or lowering once at creation) — never released,
/// caught before publish. That approximation broke the instant a second
/// toplevel was created after a `Top` panel: new toplevels appended above
/// every existing sibling, including that panel, and
/// [`crate::Runtime::raise_toplevel`] raised a toplevel above every sibling
/// too, `Top`/`Overlay` layer surfaces included — exactly the
/// panel-above-windows case a real compositor needs, which is why it never
/// shipped. The banded-tree design has no such failure mode: a `Top`/`Overlay`
/// layer surface's node can never become a
/// descendant of the toplevel band, and a toplevel's node can never become
/// a descendant of `Top`/`Overlay`, regardless of creation order, raise
/// calls, or how many toplevels or layer surfaces come and go afterward.
///
/// **There is no `raise_layer_surface` method, and none is needed.** Band
/// ordering is structural: a `Top`-band layer surface's node is always
/// above every toplevel-band and `Background`/`Bottom`-band node, in every
/// scene traversal, with no raise call required to establish or maintain
/// that. A `raise_layer_surface` would only ever have something to do
/// *within* a band (ordering two `Top` surfaces relative to each other),
/// which this crate does not need for its current consumer and does not
/// expose — the same "not this crate's job yet" reasoning
/// [`crate::LayerSurface::output_id`]'s own doc gives for
/// `set_layer_surface_output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    /// Below everything, typically a wallpaper.
    Background,
    /// Above `Background`, below ordinary toplevels — docks, status bars
    /// that want to sit under a maximized window.
    Bottom,
    /// Above ordinary toplevels — panels, launchers.
    Top,
    /// Above everything, including `Top` — on-screen keyboards, lock
    /// screens, notifications.
    Overlay,
}

impl Layer {
    /// Decode `zwlr_layer_shell_v1_layer`'s wire values (0–3, background to
    /// overlay), verified against the generated bindings rather than assumed.
    ///
    /// Any value outside `0..=3` maps to [`Layer::Overlay`] rather than
    /// panicking: a future protocol extension widening the enum is not this
    /// crate's concern to reject, and "unknown stacks highest" is the safer
    /// default of the two directions — a misclassified surface obscured by
    /// everything else is merely wrong, one placed on top of everything else
    /// by mistake is at least *visible*, and a compositor is better placed to
    /// notice and fix a surface it can see than to hunt for one it cannot.
    pub(crate) fn from_raw(v: u32) -> Layer {
        match v {
            0 => Layer::Background,
            1 => Layer::Bottom,
            2 => Layer::Top,
            _ => Layer::Overlay,
        }
    }
}

/// Which edge(s) of the output a layer surface is anchored to.
///
/// Same shape as [`crate::Edges`], and for the same reason: wlr-layer-shell's
/// `anchor` is a small, closed, protocol-frozen bitmask, and four named
/// fields make a check at the call site (`anchor.top`) read straight through
/// with no bit constant to look up. Unlike `Edges`, more than two bits may be
/// set at once for a perfectly ordinary reason — a surface anchored to all
/// four edges is stretched to fill the output, which is exactly how a
/// wallpaper or a full-width bar anchors itself — so no "at most one axis"
/// invariant is implied or checked here.
///
/// `Default` is every field `false` — anchored to no edge, which
/// wlr-layer-shell treats as "centered, sized to the surface's own desired
/// size" — matching [`Edges`](crate::Edges)' own `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Anchor {
    /// Anchored to the output's top edge.
    pub top: bool,
    /// Anchored to the output's bottom edge.
    pub bottom: bool,
    /// Anchored to the output's left edge.
    pub left: bool,
    /// Anchored to the output's right edge.
    pub right: bool,
}

impl Anchor {
    /// Decode `zwlr_layer_surface_v1_anchor`'s bitmask: `top` = 1, `bottom` =
    /// 2, `left` = 4, `right` = 8, verified against the generated protocol
    /// header (`wlr_layer_surface_v1_state::anchor` itself is a plain `u32`
    /// in the bindings — wlroots declares the field as `uint32_t`, not the
    /// enum type, so there is no `sys::` symbol to check this module's own
    /// tests against; see this module's tests for the fuller note). Unknown
    /// bits are ignored, the same "not this crate's concern to reject"
    /// reasoning [`Edges::from_xdg`](crate::Edges::from_xdg) documents for
    /// its own bitmask.
    pub(crate) fn from_bits(bits: u32) -> Anchor {
        let (top, bottom, left, right) = crate::toplevel::decode_edge_bits(bits);
        Anchor {
            top,
            bottom,
            left,
            right,
        }
    }
}

/// A wlr-layer-shell surface, borrowed for the duration of a handler call.
///
/// Same shape as [`crate::Toplevel`], for the same reason: a
/// `wlr_layer_surface_v1` is freed whenever its client says so, so a handle
/// that escapes the handler it was passed to is a use-after-free. The
/// lifetime and the private constructor make that a compile error. The id is
/// attached with `wlr_addon` to the layer surface's **surface**, mirroring
/// [`crate::ToplevelId`]'s own placement and for the identical reason: the
/// role object has no addon set of its own, `wlr_surface` does, and the two
/// die together.
pub struct LayerSurface<'h> {
    raw: NonNull<sys::wlr_layer_surface_v1>,
    id: LayerSurfaceId,
    _scope: PhantomData<&'h ()>,
}

/// Hand-written rather than derived, for the same reason
/// [`Toplevel`](crate::Toplevel)'s is: the `PhantomData` scope marker has no
/// value to print, and a raw pointer printed by a derive is neither useful
/// nor stable across runs.
impl std::fmt::Debug for LayerSurface<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayerSurface")
            .field("id", &self.id)
            .field("layer", &self.layer())
            .field("anchor", &self.anchor())
            .field("output_id", &self.output_id())
            .finish()
    }
}

impl<'h> LayerSurface<'h> {
    /// # Safety
    ///
    /// `raw` must be a live `wlr_layer_surface_v1` whose surface carries the
    /// id addon that produced `id`, and the returned handle must not outlive
    /// the callback it was created for.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_layer_surface_v1,
        id: LayerSurfaceId,
    ) -> LayerSurface<'h> {
        LayerSurface {
            raw: NonNull::new(raw).expect("wlroots handed us a null layer surface"),
            id,
            _scope: PhantomData,
        }
    }

    /// This layer surface's stable identity, safe to store beyond the
    /// handler.
    pub fn id(&self) -> LayerSurfaceId {
        self.id
    }

    /// The output the client asked to be placed on, if it named one.
    ///
    /// `None` when the client left `output` unset in its
    /// `get_layer_surface` request — wlr-layer-shell's own doc for
    /// `wlr_layer_shell_v1::events::new_surface` puts it plainly: "the
    /// output may be NULL. In this case, it is your responsibility to assign
    /// an output before returning" — so `None` here is the ordinary case a
    /// compositor is expected to handle, not an edge case.
    ///
    /// Also `None`, defensively, if `output` is non-null but this crate
    /// cannot find an id for it — which should not happen (every output this
    /// crate has announced already carries one; see `backend.rs`'s
    /// `on_new_output`) but is not treated as a panic-worthy invariant
    /// violation in a method with no way to report one.
    ///
    /// **Gap, deferred rather than fixed:** wlr-layer-shell's own doc names
    /// the compositor's responsibility plainly — "it is your responsibility
    /// to assign an output before returning" — but this crate exposes no
    /// `set_layer_surface_output`, so a consumer that receives `None` here
    /// has no way through this crate to discharge that responsibility. This
    /// is safe for this crate's own manual-positioning model specifically
    /// because nothing in this crate ever dereferences `(*ls).output`
    /// itself — `output_id` only reads and null-checks it — so a layer
    /// surface left without one simply reports `None` forever rather than
    /// crashing anything. `set_layer_surface_output` remains a real gap in
    /// the frozen surface, additive and left for a future release, not a
    /// defect being silently frozen away.
    pub fn output_id(&self) -> Option<OutputId> {
        // SAFETY: the handle's lifetime guarantees the layer surface is
        // live; `output` is read and null-checked before use, and when
        // non-null this crate's own `on_new_output` guarantees its addon set
        // is initialised.
        unsafe {
            let output = (*self.raw.as_ptr()).output;
            if output.is_null() {
                return None;
            }
            find_id(&raw const (*output).addons).map(OutputId)
        }
    }

    /// Which stacking band the client asked for.
    ///
    /// Read from `pending` rather than `current`: wlr-layer-shell's own doc
    /// says the client has already stated its anchors, layer and margins by
    /// the time `new_surface` fires, and that is before any commit has
    /// landed — so `current`, which only reflects what has actually been
    /// committed, would read the layer's zero value (`Background`) for every
    /// surface until its first commit, rather than what the client actually
    /// requested.
    pub fn layer(&self) -> Layer {
        // SAFETY: as `output_id`.
        unsafe { Layer::from_raw((*self.raw.as_ptr()).pending.layer.0) }
    }

    /// Which edge(s) of the output the client asked to anchor to. Read from
    /// `pending`, for the same reason [`layer`](LayerSurface::layer) is.
    pub fn anchor(&self) -> Anchor {
        // SAFETY: as `output_id`.
        unsafe { Anchor::from_bits((*self.raw.as_ptr()).pending.anchor) }
    }

    /// The space, in surface-local pixels, the client asked the compositor to
    /// reserve for this surface along its anchored edge. wlr-layer-shell
    /// defines `0` as "reserve nothing" and *any negative value* as "the
    /// client would not like to be moved to avoid occluding surfaces with a
    /// positive exclusive zone" — `-1` is the conventional negative value to
    /// send, but it is not privileged by the protocol over `-2` or any other
    /// negative number; a positive value is the ordinary "reserve this many
    /// pixels" case, a bar's height, say. This crate passes the raw value
    /// through rather than collapsing every negative value into one, since a
    /// caller that cares about the exact number a client sent needs it
    /// intact. A positive value only has a defined edge to apply
    /// to for certain anchor configurations — wlroots'
    /// `wlr_layer_surface_v1_get_exclusive_edge` reports `WLR_EDGE_NONE` for
    /// a nonpositive zone regardless of anchor. Read from `pending`, for the
    /// same reason [`layer`](LayerSurface::layer) is.
    pub fn exclusive_zone(&self) -> i32 {
        // SAFETY: as `output_id`.
        unsafe { (*self.raw.as_ptr()).pending.exclusive_zone }
    }

    /// The client's requested size, in surface-local pixels. Either
    /// component may be `0`, which wlr-layer-shell defines as "let the
    /// compositor decide" for that axis. Read from `pending`, for the same
    /// reason [`layer`](LayerSurface::layer) is.
    ///
    /// **Neither component is bounded above.** This is the client's raw,
    /// unvalidated `desired_width`/`desired_height`, and wlr-layer-shell puts
    /// no ceiling on either — a client can send anything up to `u32::MAX`.
    /// Combined with the `0`-means-"decide" case, that means a caller must
    /// treat this as an unclamped range on *both* ends before feeding it into
    /// signed arithmetic: casting an unbounded `u32` to `i32` (or subtracting
    /// it from one) can go negative or overflow once the value nears 2^31,
    /// which — if that happens inside a wlroots callback — aborts the whole
    /// compositor process rather than merely erroring. Clamp to a sane
    /// maximum (the output's own dimension, or a fixed cap well under
    /// `i32::MAX`) before use; see `examples/layers.rs`'s
    /// `new_layer_surface` for the pattern.
    pub fn desired_size(&self) -> (u32, u32) {
        // SAFETY: as `output_id`.
        unsafe {
            let pending = &(*self.raw.as_ptr()).pending;
            (pending.desired_width, pending.desired_height)
        }
    }

    /// Whether this surface currently wants keyboard focus at all —
    /// `exclusive` or `on_demand`, either of which the client can be given;
    /// `false` only for `none`, wlr-layer-shell's default.
    ///
    /// Read from `current.keyboard_interactive`, which the protocol defines
    /// as an enum with three values rather than a bool
    /// (`none` = 0, `exclusive` = 1, `on_demand` = 2), so this is `!= 0`
    /// rather than a single equality check — collapsing the two "wants
    /// focus" values into one `bool` here, since neither this crate nor
    /// [`crate::Runtime::focus_layer_keyboard`] treats an `on_demand`
    /// request any differently from an `exclusive` one; a compositor that
    /// needs to tell them apart reaches into the distinction itself rather
    /// than through this accessor.
    ///
    /// `current`, not `pending`, unlike every other accessor on this type:
    /// this is what the brief for this method names explicitly, and unlike
    /// layer/anchor/exclusive-zone/size — which a compositor needs at
    /// `new_surface`, before any commit exists — keyboard interactivity only
    /// matters once a surface is actually being considered for focus, which
    /// cannot happen before it is mapped, which cannot happen before at
    /// least one commit.
    ///
    /// **Reads as `false` for every surface when called from
    /// [`crate::ToplevelHandler::new_layer_surface`].** `current` is
    /// entirely zeroed until the surface's first commit —
    /// `zwlr_layer_shell_v1.get_layer_surface` only ever writes
    /// `pending.layer` at creation, confirmed by disassembling
    /// `libwlroots-0.20.so` — so this always reports `false` there
    /// regardless of what the client asked for. Call this from
    /// [`crate::ToplevelHandler::layer_surface_commit`] instead, where
    /// `current` has just been populated by the commit that triggered the
    /// call; this crate's own `examples/layers.rs` originally called it from
    /// `new_layer_surface` and its keyboard-focus path was consequently dead
    /// code until that was fixed.
    pub fn keyboard_interactive(&self) -> bool {
        // SAFETY: as `output_id`.
        unsafe { (*self.raw.as_ptr()).current.keyboard_interactive.0 != 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_from_raw_decodes_every_defined_value() {
        assert_eq!(Layer::from_raw(0), Layer::Background);
        assert_eq!(Layer::from_raw(1), Layer::Bottom);
        assert_eq!(Layer::from_raw(2), Layer::Top);
        assert_eq!(Layer::from_raw(3), Layer::Overlay);
    }

    /// The frozen, tested-not-assumed contract: out of range never panics
    /// and always lands on `Overlay`, the "stack it highest, where it can at
    /// least be seen" default this type's own doc argues for.
    #[test]
    fn layer_from_raw_never_panics_and_defaults_out_of_range_to_overlay() {
        for v in [4u32, 5, 255, u32::MAX] {
            assert_eq!(Layer::from_raw(v), Layer::Overlay, "v = {v}");
        }
    }

    /// `wlr_layer_surface_v1_state::anchor` is a plain `u32` in the generated
    /// bindings — wlroots' own header declares the field as `uint32_t`, not
    /// as `enum zwlr_layer_surface_v1_anchor`, so bindgen has no enum type
    /// here to bind and there is no `sys::` constant this test could check
    /// itself against. The bit values below are transcribed from
    /// `wlr-layer-shell-unstable-v1-protocol.h`'s own
    /// `enum zwlr_layer_surface_v1_anchor` (verified by hand against the
    /// generated protocol header this build produced:
    /// `ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP = 1`, `_BOTTOM = 2`, `_LEFT = 4`,
    /// `_RIGHT = 8`) rather than against a symbol, which is the best this
    /// crate can do for a value with no corresponding Rust binding to pin
    /// against. This test exists so a `from_bits` change is at least
    /// self-consistent with the comment even though neither can be checked
    /// against the other automatically.
    #[test]
    fn anchor_from_bits_decodes_each_bit_independently_and_together() {
        assert_eq!(Anchor::from_bits(0), Anchor::default());
        assert_eq!(
            Anchor::from_bits(1),
            Anchor {
                top: true,
                ..Default::default()
            }
        );
        assert_eq!(
            Anchor::from_bits(2),
            Anchor {
                bottom: true,
                ..Default::default()
            }
        );
        assert_eq!(
            Anchor::from_bits(4),
            Anchor {
                left: true,
                ..Default::default()
            }
        );
        assert_eq!(
            Anchor::from_bits(8),
            Anchor {
                right: true,
                ..Default::default()
            }
        );
        // Stretched across the whole output: every bit set at once.
        assert_eq!(
            Anchor::from_bits(1 | 2 | 4 | 8),
            Anchor {
                top: true,
                bottom: true,
                left: true,
                right: true,
            }
        );
    }

    #[test]
    fn dangling_for_test_is_far_outside_the_real_id_space() {
        assert!(LayerSurfaceId::dangling_for_test().0 > u32::MAX as u64);
    }
}
