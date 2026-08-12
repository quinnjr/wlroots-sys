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
//! notices nothing has been staged or sent for it and gives
//! [`ToplevelHandler::request_decoration_mode`]
//! (`crate::ToplevelHandler::request_decoration_mode`) its say right there,
//! with `client_side_preferred: None` — "stated no preference", which is
//! exactly true of a client that never asked.
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
/// Both are `Cell`s, not plain fields, because every accessor reaches them
/// through a shared `&Runtime`, never a `&mut`.
pub(crate) struct DecorationEntry {
    pub(crate) raw: NonNull<sys::wlr_xdg_toplevel_decoration_v1>,
    pub(crate) mode_set_this_dispatch: Cell<bool>,
    pub(crate) staged: Cell<Option<bool>>,
}

/// Read `wlr_xdg_toplevel_decoration_v1::requested_mode` into the shape
/// [`crate::ToplevelHandler::request_decoration_mode`] hands a consumer.
///
/// `0` (`WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_NONE`) is "no preference
/// stated" — a client that has not yet called `set_mode` at all, which the
/// protocol allows — and maps to `None` rather than to either bool, since
/// neither `Some(true)` nor `Some(false)` would be honest about a client
/// that asked for nothing.
pub(crate) fn client_side_preference(
    mode: sys::wlr_xdg_toplevel_decoration_v1_mode,
) -> Option<bool> {
    match mode.0 {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}
