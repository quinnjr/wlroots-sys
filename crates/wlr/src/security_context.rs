//! The `wp_security_context_v1` manager: sandbox metadata attached to a
//! connection.
//!
//! A sandbox engine binds `wp_security_context_manager_v1`, creates a
//! `wp_security_context_v1` over a listening socket, attaches a sandbox-engine
//! name, application id and instance id, and commits. The compositor observes
//! the commit, and can later resolve the metadata for any of the connections
//! the sandbox accepted by calling
//! [`Runtime::lookup_security_context`].
//!
//! This module wraps the manager global and the committed metadata. wlroots
//! hands the commit signal a `wlr_security_context_v1_commit_event` whose
//! `state` pointer is owned by a context object that lives and dies with its
//! client, so the crate copies the three strings out at emission time and
//! delivers an owned [`SecurityContext`] to
//! [`crate::ToplevelHandler::security_context_committed`]. The committed value
//! therefore outlives the client that produced it.

use std::ptr::NonNull;

use crate::runtime::copy_nullable_string;
use crate::{Display, Error, Result, Runtime, sys};

/// The metadata a committed `wp_security_context_v1` carried.
///
/// Every field is owned, copied out of wlroots at commit (or lookup) time, so
/// the value is safe to keep for as long as the caller likes. Each is `None`
/// when the client did not set it — all three are optional in the protocol,
/// though a well-behaved sandbox engine sets the sandbox-engine name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecurityContextState {
    sandbox_engine: Option<String>,
    app_id: Option<String>,
    instance_id: Option<String>,
}

impl SecurityContextState {
    /// The sandbox engine's reverse-DNS name, e.g. `"org.flatpak"`.
    pub fn sandbox_engine(&self) -> Option<&str> {
        self.sandbox_engine.as_deref()
    }

    /// The sandbox-specific application id.
    pub fn app_id(&self) -> Option<&str> {
        self.app_id.as_deref()
    }

    /// The sandbox-specific running-instance id.
    pub fn instance_id(&self) -> Option<&str> {
        self.instance_id.as_deref()
    }
}

/// A committed security context, as delivered to
/// [`crate::ToplevelHandler::security_context_committed`] and returned by
/// [`Runtime::lookup_security_context`].
///
/// The metadata is owned ([`state`](Self::state)), so a consumer can store the
/// whole value and read it after the client and the display are gone. The
/// convenience accessors forward to [`state`](Self::state).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecurityContext {
    state: SecurityContextState,
}

impl SecurityContext {
    /// The context's metadata.
    pub fn state(&self) -> &SecurityContextState {
        &self.state
    }

    /// The sandbox engine's reverse-DNS name; see
    /// [`SecurityContextState::sandbox_engine`].
    pub fn sandbox_engine(&self) -> Option<&str> {
        self.state.sandbox_engine()
    }

    /// The sandbox-specific application id; see
    /// [`SecurityContextState::app_id`].
    pub fn app_id(&self) -> Option<&str> {
        self.state.app_id()
    }

    /// The sandbox-specific running-instance id; see
    /// [`SecurityContextState::instance_id`].
    pub fn instance_id(&self) -> Option<&str> {
        self.state.instance_id()
    }
}

/// Copy a live security context's state out into an owned snapshot.
///
/// # Safety
///
/// `state` must point at a live `wlr_security_context_v1_state`; each of its
/// string fields must be null or a live NUL-terminated string.
pub(crate) unsafe fn snapshot(state: *const sys::wlr_security_context_v1_state) -> SecurityContext {
    // SAFETY: the caller guarantees `state` is live, and every field read is a
    // null-or-NUL-terminated `char *` copied out here.
    let state = unsafe {
        SecurityContextState {
            sandbox_engine: copy_nullable_string((*state).sandbox_engine),
            app_id: copy_nullable_string((*state).app_id),
            instance_id: copy_nullable_string((*state).instance_id),
        }
    };
    SecurityContext { state }
}

impl Runtime {
    /// Create the `wp_security_context_v1` global, letting a sandbox engine
    /// attach security metadata to the connections it spawns. Errors if called
    /// twice.
    ///
    /// The manager lives and dies with `display`; this crate never frees it. A
    /// client's `commit` reaches
    /// [`crate::ToplevelHandler::security_context_committed`] once a
    /// [`crate::Backend::run_all`](crate::Backend::run_all) has linked the
    /// signal — create the manager before the run.
    pub fn create_security_context_manager(&self, display: &Display) -> Result<()> {
        if self.inner.security_context_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_security_context_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; wlroots owns the returned
        // manager and frees it with the display.
        let raw = unsafe { sys::wlr_security_context_manager_v1_create(display.as_ptr()) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_security_context_manager_v1_create"))?;
        *self.inner.security_context_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The security-context manager, once created — read by `backend.rs`'s
    /// manager wiring to link the `commit` listener.
    pub(crate) fn security_context_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_security_context_manager_v1>> {
        *self.inner.security_context_manager.borrow()
    }

    /// Look up the security context attached to `client`, copying its metadata
    /// out.
    ///
    /// This is what a compositor calls from its own Wayland global filter: the
    /// filter receives the client of a connection the sandbox accepted, and
    /// this answers whether that connection carries a security context and, if
    /// so, what metadata. `None` when no manager was created, or when `client`
    /// has no attached context.
    ///
    /// # Safety
    ///
    /// `client` must be null or a live `wl_client`. wlroots keys the lookup on
    /// the client with `wl_client_get_destroy_listener`, so a dangling pointer
    /// would be dereferenced; null is refused here before that call.
    pub unsafe fn lookup_security_context(
        &self,
        client: *const sys::wl_client,
    ) -> Option<SecurityContext> {
        let manager = self.security_context_manager_ptr()?;
        let client = NonNull::new(client.cast_mut())?;
        // SAFETY: `manager` is live (display-owned, and the runtime keeps the
        // display alive), and `client` is live per the caller's contract. The
        // returned state pointer is owned by the context object and valid until
        // its client is destroyed; the snapshot copies every field out before
        // returning.
        let state = unsafe {
            sys::wlr_security_context_manager_v1_lookup_client(manager.as_ptr(), client.as_ptr())
        };
        if state.is_null() {
            return None;
        }
        // SAFETY: the lookup returned a non-null live state pointer.
        Some(unsafe { snapshot(state) })
    }
}
