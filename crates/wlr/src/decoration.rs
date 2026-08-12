//! xdg-decoration negotiation: server-side vs. client-side window chrome.
//!
//! A `wlr_xdg_toplevel_decoration_v1` is a protocol object, not a scene
//! object — it carries no pixels and nothing to render, only the client's
//! stated preference and the compositor's answer. That answer has to be
//! sent before the toplevel's initial commit is acknowledged (wlroots
//! requires a mode be set before then), so this crate answers it
//! unconditionally: [`ToplevelHandler::request_decoration_mode`]
//! (`crate::ToplevelHandler::request_decoration_mode`) gets first say, and
//! if it says nothing, the dispatch layer defaults to server-side — the same
//! "the handler may stage nothing and the protocol still gets answered"
//! shape `ToplevelHandler::request_maximize` already documents for its own
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

/// A live decoration object: the pointer, and whether
/// [`crate::Runtime::set_decoration_mode`] has already answered the request
/// currently in flight for it.
///
/// The flag is what turns "the handler may call `set_decoration_mode`, or
/// may not" into "exactly one `wlr_xdg_toplevel_decoration_v1_set_mode` call
/// per `request_mode` event": `backend.rs`'s `on_decoration_request_mode`
/// clears it right before delivering the event, the handler's own call to
/// `set_decoration_mode` (if any) sets it, and `deliver_all` checks it once
/// the handler returns — sending the server-side default itself only if the
/// handler left it clear. A `Cell`, not a plain `bool`, because both the
/// clear/check pair and `set_decoration_mode` reach it through a shared
/// `&Runtime`, never a `&mut`.
pub(crate) struct DecorationEntry {
    pub(crate) raw: NonNull<sys::wlr_xdg_toplevel_decoration_v1>,
    pub(crate) mode_set_this_dispatch: Cell<bool>,
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
