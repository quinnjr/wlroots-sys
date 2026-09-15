//! The `xdg_system_bell_v1` manager: a client's request that the compositor
//! ring the system bell.
//!
//! The protocol exists for notification daemons and terminal emulators that
//! want the compositor — not the client — to make the audible alert. A client
//! calls `ring`, optionally naming a surface the alert is about; wlroots
//! forwards that as its `ring` signal, and this crate delivers it to
//! [`crate::ToplevelHandler::system_bell_ring`]. Making a sound is entirely the
//! compositor's call; a compositor that does nothing is conforming.
//!
//! The manager global is display-owned, so this module is just the create call
//! and the pointer the run wiring reads.

use std::ptr::NonNull;

use crate::{Display, Error, Result, Runtime, sys};

impl Runtime {
    /// Create the `xdg_system_bell_v1` global, letting clients ask the
    /// compositor to ring the system bell. Errors if called twice.
    ///
    /// `version` is the protocol version to advertise; the interface currently
    /// has one version. A client's `ring` reaches
    /// [`crate::ToplevelHandler::system_bell_ring`] once a
    /// [`crate::Backend::run_all`](crate::Backend::run_all) has linked the
    /// signal — create the manager before the run.
    pub fn create_xdg_system_bell(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.xdg_system_bell.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_xdg_system_bell called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_xdg_system_bell_v1_create(display.as_ptr(), version) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_xdg_system_bell_v1_create"))?;
        *self.inner.xdg_system_bell.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `xdg_system_bell_v1` manager, once created via
    /// [`Runtime::create_xdg_system_bell`] — read by `backend.rs`'s
    /// `register_toplevel_and_input` to link the `ring` listener.
    pub(crate) fn xdg_system_bell_ptr(&self) -> Option<NonNull<sys::wlr_xdg_system_bell_v1>> {
        *self.inner.xdg_system_bell.borrow()
    }
}
