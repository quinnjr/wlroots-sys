//! wlr-layer-shell-unstable-v1: panels, launchers, wallpapers and other
//! surfaces anchored to an output's edges rather than positioned by the
//! compositor's own window management, added to this crate in 0.20.11.
//!
//! # The timing hazard, investigated
//!
//! xdg-decoration's `set_decoration_mode` (see `decoration.rs`'s own doc)
//! stages its answer instead of sending it immediately because
//! `wlr_xdg_surface_schedule_configure` asserts `surface->initialized`, and
//! that flag only flips true during the toplevel's first commit — so a
//! preference the client stated before ever committing (the ordinary case)
//! cannot be answered synchronously. `wlr_layer_surface_v1` carries the
//! identically-named fields `initialized`/`initial_commit`, with the 0.20
//! header spelling `initialized`'s purpose exactly the same way
//! ("Whether the surface is ready to receive configure events"), so the same
//! question had to be asked here before writing a line of this module:
//! does `wlr_layer_surface_v1_configure` assert on it too?
//!
//! The answer could not be confirmed by reading `wlr_layer_surface_v1_configure`
//! itself firsthand against the exact 0.20.11 sources — gitlab.freedesktop.org
//! blocks automated fetches, and the one mirror this investigation could reach
//! (a stale pre-refactor snapshot, whose `wlr_layer_surface_v1` still has the
//! older `added`/`configured`/`mapped` fields rather than `initialized`/
//! `initial_commit`, so it predates the change entirely) is not evidence
//! about the version this crate binds. What *is* evidence: the 0.20 header's
//! own doc comment on `initialized`, worded to match `wlr_xdg_surface`'s
//! identical field, and independent corroboration that current wlroots logs
//! (or asserts — the exact severity could not be confirmed either) a
//! diagnostic worded "a configure is sent to an uninitialized
//! wlr_layer_surface_v1" for exactly this misuse. That is enough to treat the
//! hazard as real rather than gamble a compositor's whole event loop on a
//! guess, so [`crate::Runtime::configure_layer_surface`] applies the identical
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
/// # A documented 0.20.x limitation: two bands, not four
///
/// This crate's scene graph places every toplevel directly under the scene
/// root (see `backend.rs`'s `on_new_toplevel`), with nothing between it and
/// the root to slot a layer surface's tree into on either side. Giving each
/// of the four layers its own scene sub-tree, with toplevels living in a
/// fifth between `Bottom` and `Top`, is the correct general fix and is not
/// done here — it is a structural change to the M1 scene this crate already
/// ships, not a layer-shell concern in isolation.
///
/// Instead, a layer surface's scene node is raised or lowered *once*, at
/// creation (`backend.rs`'s `on_new_layer_surface`): `Background` and
/// `Bottom` call `wlr_scene_node_lower_to_bottom`, `Top` and `Overlay` call
/// `wlr_scene_node_raise_to_top`. That collapses four bands into two —
/// everything below toplevels, and everything above them — which is
/// observably wrong only when two surfaces in the *same* pair (two
/// `Background`-and-`Bottom` clients, say) need a guaranteed relative order,
/// or when a `Background` surface must never be capable of ending up above a
/// `Bottom` one (both simply land at "the bottom", in creation order, via a
/// series of `lower_to_bottom` calls, and the two variants become
/// indistinguishable in the scene once placed).
///
/// This is accepted rather than fixed for 0.20.11 because icedtea M2 — the
/// only consumer this release serves — uses `Top` for its panels and its own
/// wallpaper for the background, never a client-supplied `Background`
/// surface, so the two-band approximation is invisible to it. Revisit this
/// only once a real consumer needs strict `Bottom`-vs-`Background` ordering
/// or an `Overlay` surface that must outrank a `Top` one deterministically.
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
/// size" — matching [`Edges`]' own `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
        Anchor {
            top: bits & 1 != 0,
            bottom: bits & 2 != 0,
            left: bits & 4 != 0,
            right: bits & 8 != 0,
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
    /// defines `0` as "reserve nothing" and `-1` as "this surface does not
    /// participate in exclusive-zone layout at all" (a positive value is the
    /// ordinary "reserve this many pixels" case — a bar's height, say). This
    /// crate passes the raw value through rather than collapsing the two
    /// non-reserving cases into one, since a caller that cares about the
    /// distinction (querying `wlr_layer_surface_v1_get_exclusive_edge`'s
    /// equivalent, say) needs it intact. Read from `pending`, for the same
    /// reason [`layer`](LayerSurface::layer) is.
    pub fn exclusive_zone(&self) -> i32 {
        // SAFETY: as `output_id`.
        unsafe { (*self.raw.as_ptr()).pending.exclusive_zone }
    }

    /// The client's requested size, in surface-local pixels. Either
    /// component may be `0`, which wlr-layer-shell defines as "let the
    /// compositor decide" for that axis. Read from `pending`, for the same
    /// reason [`layer`](LayerSurface::layer) is.
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
