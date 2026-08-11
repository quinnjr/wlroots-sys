//! Borrow-scoped toplevel handles and their stable ids.
//!
//! Same shape as [`Output`](crate::Output), for the same reason: a
//! `wlr_xdg_toplevel` is freed whenever its client says so, so a handle that
//! escapes the handler it was passed to is a use-after-free. The lifetime and
//! the private constructor make that a compile error.
//!
//! The id is attached with `wlr_addon` to the toplevel's **surface**, not to
//! the toplevel itself: `wlr_xdg_toplevel` has no addon set, `wlr_surface`
//! does, and the two die together (wlroots destroys the toplevel role object
//! with the surface that carries it). So wlroots runs the id's destructor at
//! exactly the right moment and nothing has to be swept.

use std::ffi::CStr;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::sys;

/// Identifies a toplevel for as long as the consumer chooses to remember it.
///
/// Storable, comparable and hashable — unlike a handle. Ids are never reused
/// within a process, and an id held past its toplevel's destruction resolves
/// to nothing rather than to another window.
///
/// Deliberately no `PartialOrd`/`Ord`: an opaque id's ordering would promise
/// creation-order semantics nobody asked for, and this API is frozen within
/// the wlroots minor, so a derive added here could not be withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToplevelId(pub(crate) u64);

impl ToplevelId {
    /// An id no live toplevel can have, for testing the "unknown id" path.
    ///
    /// Public because "every by-id operation reports a miss rather than
    /// dereferencing" is a promise to consumers, and a promise nobody can
    /// write a test for is not one. It is also the *only* way to obtain a
    /// `ToplevelId` outside a handler, the field being private — so hiding it
    /// from the docs would leave the promise untestable in practice while
    /// still freezing the function.
    ///
    /// That every by-id operation misses on this value is part of the frozen
    /// contract, not an implementation accident: ids come from a process-wide
    /// counter that starts at 1, only ever increments, and never reuses a
    /// value, so `u64::MAX` cannot be handed to a real toplevel.
    ///
    /// Not for production code. An id from a real toplevel is the one
    /// [`Toplevel::id`] returns, and it stops resolving once the
    /// [`Backend::run_all`](crate::Backend::run_all) call that announced it
    /// has returned — at which point it behaves exactly like this one.
    pub fn dangling_for_test() -> ToplevelId {
        ToplevelId(u64::MAX)
    }
}

/// An xdg toplevel, borrowed for the duration of a handler call.
pub struct Toplevel<'h> {
    raw: NonNull<sys::wlr_xdg_toplevel>,
    id: ToplevelId,
    _scope: PhantomData<&'h ()>,
}

impl<'h> Toplevel<'h> {
    /// # Safety
    ///
    /// `raw` must be a live `wlr_xdg_toplevel` whose surface carries the id
    /// addon that produced `id`, and the returned handle must not outlive the
    /// callback it was created for.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_xdg_toplevel,
        id: ToplevelId,
    ) -> Toplevel<'h> {
        Toplevel {
            raw: NonNull::new(raw).expect("wlroots handed us a null toplevel"),
            id,
            _scope: PhantomData,
        }
    }

    /// This toplevel's stable identity, safe to store beyond the handler.
    pub fn id(&self) -> ToplevelId {
        self.id
    }

    /// The client's `xdg_toplevel.set_title`, if it has sent one.
    pub fn title(&self) -> Option<String> {
        // SAFETY: the handle's lifetime guarantees the toplevel is live;
        // wlroots leaves `title` null until the client sets one.
        unsafe { cstr_field((*self.raw.as_ptr()).title) }
    }

    /// The client's `xdg_toplevel.set_app_id`, if it has sent one.
    pub fn app_id(&self) -> Option<String> {
        // SAFETY: as for `title`.
        unsafe { cstr_field((*self.raw.as_ptr()).app_id) }
    }

    /// The pid of the client that owns this toplevel.
    ///
    /// `None` when the resource has no client, which happens only for an
    /// inert resource — a toplevel whose client disconnected between wlroots
    /// queueing an event and this crate delivering it.
    pub fn pid(&self) -> Option<u32> {
        use sys::wayland_sys::ffi_dispatch;
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: the handle's lifetime guarantees the toplevel is live, so
        // its `resource` is a live `wl_resource`; both libwayland calls are
        // reads, and the out-parameters are stack locals of the right types.
        unsafe {
            let resource = (*self.raw.as_ptr()).resource;
            if resource.is_null() {
                return None;
            }
            let client = ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_resource_get_client,
                resource
            );
            if client.is_null() {
                return None;
            }
            let mut pid: sys::pid_t = 0;
            let mut uid: u32 = 0;
            let mut gid: u32 = 0;
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_client_get_credentials,
                client,
                &raw mut pid,
                &raw mut uid,
                &raw mut gid
            );
            u32::try_from(pid).ok()
        }
    }
}

/// Copy a wlroots-owned C string field out, or `None` if it is null.
///
/// # Safety
///
/// `p` must be null or a live, NUL-terminated C string owned by wlroots.
unsafe fn cstr_field(p: *mut std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `p` is a live NUL-terminated string; this
    // copies it out and never frees it.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}
