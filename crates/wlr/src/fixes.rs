//! The `wlr_fixes` global: wlroots' implementation of the core `wl_fixes`
//! protocol, the workaround for the frozen `wl_registry` interface.
//!
//! `wl_fixes` exists to settle global and registry lifetime. A client calls
//! `destroy_registry` to drop a `wl_registry` object early, and calls
//! `ack_global_remove` after a `wl_registry.global_remove` to confirm it will
//! not bind that global again. Because `wl_registry` is frozen and cannot carry
//! the ack itself, it travels on this second global — and only then can the
//! compositor safely free a withdrawn `wl_global` instead of destroying it on a
//! timer and racing a late client. A compositor that advertises it does so with
//! [`Runtime::create_fixes`] before the run; a runtime that never calls this
//! exposes nothing and clients cannot acknowledge, which is the unfixed
//! behaviour.
//!
//! The global is display-owned: wlroots creates it, links its own display
//! teardown and frees it with the display, so this crate only keeps the pointer
//! it used to discard.

use std::ptr::NonNull;

use crate::{Display, Error, Result, Runtime, sys};

impl Runtime {
    /// Create the `wlr_fixes` global, advertising wlroots' `wl_fixes`
    /// implementation. Errors if called twice.
    ///
    /// `version` is the protocol version to advertise. The global lives and
    /// dies with `display`; this crate never frees it, and there is no other
    /// call against it — advertising it is the whole API.
    pub fn create_fixes(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.fixes.borrow().is_some() {
            return Err(Error::Operation("Runtime::create_fixes called twice"));
        }
        // SAFETY: `display` is live for the call; wlroots owns the returned
        // global and its display-destroy listener frees it with the display.
        let raw = unsafe { sys::wlr_fixes_create(display.as_ptr(), version) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_fixes_create"))?;
        *self.inner.fixes.borrow_mut() = Some(raw);
        Ok(())
    }
}
