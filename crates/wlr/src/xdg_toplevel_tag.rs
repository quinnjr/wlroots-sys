//! The `xdg_toplevel_tag_manager_v1` manager: persistent per-toplevel tags.
//!
//! A client tags a toplevel so the compositor can match it again after an
//! application restart — a window rule's identity, independent of the title
//! or `app_id`, which the user may change. The tag is a short untranslated
//! string; the description is its translated counterpart, meant for display
//! or a screen reader.
//!
//! wlroots does not store either string on the toplevel — it only forwards the
//! client's request as the manager's `set_tag`/`set_description` signals — so
//! the strings are copied out at emission time and handed to
//! [`crate::ToplevelHandler::toplevel_tag_changed`] /
//! [`crate::ToplevelHandler::toplevel_description_changed`].
//!
//! The manager global is display-owned, so this module is just the create call
//! and the pointer the run wiring reads.

use std::ptr::NonNull;

use crate::{Display, Error, Result, Runtime, sys};

impl Runtime {
    /// Create the `xdg_toplevel_tag_manager_v1` global, letting clients tag a
    /// toplevel for persistence. Errors if called twice.
    ///
    /// `version` is the protocol version to advertise; the interface currently
    /// has one version. A client's `set_toplevel_tag`/`set_toplevel_description`
    /// reach [`crate::ToplevelHandler::toplevel_tag_changed`] /
    /// [`crate::ToplevelHandler::toplevel_description_changed`] once a
    /// [`crate::Backend::run_all`](crate::Backend::run_all) has linked the
    /// signals — create the manager before the run.
    pub fn create_xdg_toplevel_tag_manager(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.xdg_toplevel_tag_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_xdg_toplevel_tag_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_xdg_toplevel_tag_manager_v1_create(display.as_ptr(), version) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_xdg_toplevel_tag_manager_v1_create"))?;
        *self.inner.xdg_toplevel_tag_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `xdg_toplevel_tag_manager_v1` manager, once created via
    /// [`Runtime::create_xdg_toplevel_tag_manager`] — read by `backend.rs`'s
    /// `register_toplevel_and_input` to link the `set_tag`/`set_description`
    /// listeners.
    pub(crate) fn xdg_toplevel_tag_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_xdg_toplevel_tag_manager_v1>> {
        *self.inner.xdg_toplevel_tag_manager.borrow()
    }
}
