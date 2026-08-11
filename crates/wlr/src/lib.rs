//! Safe bindings to wlroots.
//!
//! See `docs/superpowers/specs/2026-08-03-wlr-safe-wrapper-design.md` for the
//! ownership model this crate is built around.

pub(crate) mod sys;

mod dispatch;
mod error;
mod id;

pub use error::{Error, Result};
pub use id::OutputId;

/// The wlroots version this build of `wlr` binds, as `(major, minor)`.
///
/// Read from `wlr-sys`'s own header constants rather than from this crate's
/// version, so a dependency that does not match the branch is observable
/// instead of silent.
pub fn wlroots_version() -> (u32, u32) {
    (sys::WLR_VERSION_MAJOR, sys::WLR_VERSION_MINOR)
}
