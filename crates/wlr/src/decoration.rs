//! xdg-decoration negotiation: server-side vs. client-side window chrome.
//!
//! A `wlr_xdg_toplevel_decoration_v1` is a protocol object, not a scene
//! object — it carries no pixels and nothing to render, only the client's
//! stated preference and the compositor's answer. That answer has to be
//! *sent* — a real `wlr_xdg_toplevel_decoration_v1_set_mode` call — before
//! the toplevel's initial commit is acknowledged, but it cannot be sent
//! before that surface's first role commit lands: wlroots asserts
//! `surface->initialized` inside the `wlr_xdg_surface_schedule_configure`
//! call `set_mode` makes internally, and that flag only flips true *during*
//! the first commit. The two constraints together are why this module
//! stages rather than sends whenever a decision arrives too early:
//! [`crate::Runtime::set_decoration_mode`] records the choice in
//! [`DecorationEntry::staged`] instead of calling into wlroots, and
//! `backend.rs`'s `on_surface_commit` flushes it — for real, now that the
//! surface is initialized — the moment the toplevel's initial commit is
//! processed. A decoration whose client never calls `set_mode` at all is
//! handled the same way, from the other direction: `on_surface_commit`
//! notices nothing has ever been answered for it and gives
//! [`ToplevelHandler::request_decoration_mode`]
//! (`crate::ToplevelHandler::request_decoration_mode`) its say right there,
//! with `preference: None` — "stated no preference", which is exactly true
//! of a client that never asked. A decoration created *after* that initial
//! commit gets the same treatment from `on_new_toplevel_decoration`, which
//! is the only other moment it could be reached: the commit that would have
//! caught it has already happened, so without that second trigger a client
//! that creates a decoration late and never calls `set_mode` would wait
//! forever for a configure that nothing was going to send.
//!
//! Either way, the handler gets first say, and if it says nothing, the
//! dispatch layer defaults to server-side — the same "the handler may stage
//! nothing and the protocol still gets answered" shape
//! `ToplevelHandler::request_maximize` already documents for its own
//! configure.
//!
//! One decoration object may exist per toplevel (the client is the one that
//! creates it, via `zxdg_decoration_manager_v1.get_toplevel_decoration`), so
//! this crate keys its table by [`crate::ToplevelId`] rather than minting a
//! new id type for the decoration itself — nothing here ever hands a
//! consumer a decoration handle to remember instead.

use std::cell::Cell;
use std::ptr::NonNull;

use crate::sys;

/// Which side draws a toplevel's window chrome.
///
/// Both halves of the negotiation speak this type — the client's stated
/// preference in
/// [`ToplevelHandler::request_decoration_mode`](crate::ToplevelHandler::request_decoration_mode)
/// and the compositor's answer in
/// [`Runtime::set_decoration_mode`](crate::Runtime::set_decoration_mode) —
/// which is the point of it existing rather than a `bool` on each side.
/// 0.20.8 used a `bool` for both, and the two had *opposite* polarity: the
/// preference's `true` meant client-side while the answer's `true` meant
/// server-side, so honoring the client read as passing the value straight
/// through when it actually required negating it. That version is yanked;
/// see this crate's README.
///
/// "No preference" is not a variant here. The protocol's third mode value
/// (`WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_NONE`) means the client has not
/// asked for anything yet, which is the absence of a preference rather than
/// a third kind of chrome — so it is spelled `Option::None` in the
/// `Option<DecorationMode>` the handler receives, and cannot be named where
/// a decision is required.
///
/// Deliberately **not** `#[non_exhaustive]`, which is a decision rather than
/// an omission: it can only be added in a breaking change, so it is now or
/// never. zxdg-decoration-v1 defines exactly these two decorated modes and
/// has been stable for years; a third would need a new protocol version, and
/// naming it on the answering side would be a breaking change with or
/// without the attribute. Forward-compatibility only really matters on the
/// *reading* side, and this module's `requested_preference` already absorbs an
/// unrecognized wire value as `None` rather than needing a variant for it.
/// So `#[non_exhaustive]` would buy nothing here and cost every consumer a
/// permanently-unreachable `_ =>` arm on a two-element match.
///
/// No `Default`, for the same reason 0.20.8 was yanked: server-side is the
/// *dispatch layer's* policy for a silent handler, not a property of this
/// type, and a `Default` would quietly reintroduce "the value you get when
/// you did not think about it."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecorationMode {
    /// The client draws its own titlebar and borders.
    ClientSide,
    /// The compositor draws the window chrome.
    ServerSide,
}

impl DecorationMode {
    /// The wire value wlroots wants, per the zxdg_decoration_v1 protocol:
    /// `1` = client-side, `2` = server-side.
    pub(crate) fn to_raw(self) -> sys::wlr_xdg_toplevel_decoration_v1_mode {
        match self {
            DecorationMode::ClientSide => {
                sys::wlr_xdg_toplevel_decoration_v1_mode::WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_CLIENT_SIDE
            }
            DecorationMode::ServerSide => {
                sys::wlr_xdg_toplevel_decoration_v1_mode::WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE
            }
        }
    }
}

/// A live decoration object: the pointer, whether
/// [`crate::Runtime::set_decoration_mode`] has already answered the request
/// currently in flight for it, and any decision still waiting for the
/// surface to be safe to configure.
///
/// `mode_set_this_dispatch` is what turns "the handler may call
/// `set_decoration_mode`, or may not" into "exactly one decision per
/// `request_mode` event": `deliver_all`'s `RequestDecorationMode` arm
/// clears it immediately before calling the handler, the handler's own call
/// to `set_decoration_mode` (if any) sets it, and the same arm checks it
/// once the handler returns — applying the server-side default itself only
/// if the handler left it clear. Clearing happens in `deliver_all` rather
/// than in the raw callback that builds the event specifically so the clear
/// and the check bracket one handler invocation even under deferral: a
/// clear at emit time, with delivery only sometimes immediate, could pair
/// with the wrong event's check.
///
/// `staged` is the other half of the mechanism, and independent of it:
/// `set_decoration_mode` writes here instead of calling into wlroots
/// whenever the toplevel's surface is not yet initialized (see this
/// module's own doc), overwriting whatever was staged before — last write
/// wins, coalesced into one eventual `set_mode` call, the same shape every
/// `Runtime::set_toplevel_*` staging setter already has for the base
/// configure. `on_surface_commit` takes it at the toplevel's initial
/// commit and actually sends it.
///
/// `answered` is the third, and the coarsest: it latches true the first time
/// *any* mode is set for this decoration — staged or sent, by the handler or
/// by the dispatch layer's default — and never clears while the decoration
/// lives, including across an unmap/remap cycle. That last part is
/// deliberate. An unmap does not destroy the decoration (only
/// `Runtime::forget_decoration`/`forget_toplevel` drop the entry, and
/// neither runs for an unmap), so if a remap produces a second
/// `initial_commit` this flag is still set and the synthetic
/// "the client never asked" request is correctly skipped: the mode was
/// negotiated, sent, and has not been withdrawn, so the decoration really
/// has been answered. Clearing on unmap would instead re-ask with
/// `preference: None` on every remap and let the server-side default
/// overwrite a client-side decision the compositor had already made.
/// Nothing hangs either way — `on_surface_commit` schedules the
/// xdg_surface configure that actually unblocks the client unconditionally
/// at every initial commit, decoration or not — and a client that genuinely
/// wants to renegotiate calls `set_mode`, which reaches
/// `on_decoration_request_mode` and is never gated on this flag at all. `mode_set_this_dispatch` scopes to one handler invocation and
/// `staged` clears the moment it is flushed, so neither can answer "has this
/// decoration ever been given a mode at all?"; that question is what
/// `on_surface_commit` and `on_new_toplevel_decoration` need before firing
/// the synthetic "the client never asked" event, since firing it for a
/// decoration already answered would override the standing decision with the
/// server-side default. 0.20.8 asked `staged.is_none()` there instead, which
/// is a different question with the same answer only when nothing was ever
/// sent immediately — so a mode chosen from `initial_commit` (where the
/// surface is already initialized, so `set_decoration_mode` sends and clears
/// `staged`) was silently overridden. Latching, rather than clearing per
/// dispatch, is what makes the guard hold across every later commit too.
///
/// All three are `Cell`s, not plain fields, because every accessor reaches
/// them through a shared `&Runtime`, never a `&mut`.
pub(crate) struct DecorationEntry {
    pub(crate) raw: NonNull<sys::wlr_xdg_toplevel_decoration_v1>,
    pub(crate) mode_set_this_dispatch: Cell<bool>,
    pub(crate) staged: Cell<Option<DecorationMode>>,
    pub(crate) answered: Cell<bool>,
}

/// Read `wlr_xdg_toplevel_decoration_v1::requested_mode` into the shape
/// [`crate::ToplevelHandler::request_decoration_mode`] hands a consumer.
///
/// `0` (`WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_NONE`) is "no preference
/// stated" — a client that has not yet called `set_mode` at all, which the
/// protocol allows — and maps to `None` rather than to either variant, since
/// neither would be honest about a client that asked for nothing.
///
/// Any other value maps to `None` too. The protocol defines only these
/// three, so a fourth can only come from a wlroots that has grown one, and
/// reporting an unrecognized mode as "stated no preference" leaves the
/// compositor to decide — the same outcome as a client that stayed silent,
/// and the only honest reading of a value this crate cannot interpret.
pub(crate) fn requested_preference(
    mode: sys::wlr_xdg_toplevel_decoration_v1_mode,
) -> Option<DecorationMode> {
    match mode.0 {
        1 => Some(DecorationMode::ClientSide),
        2 => Some(DecorationMode::ServerSide),
        _ => None,
    }
}
