//! `ext_workspace_v1`: a compositor lists its workspaces (virtual desktops) and
//! workspace groups, and a taskbar or dock observes and drives them.
//!
//! The compositor owns two kinds of object, each an owned handle:
//!
//! * [`WorkspaceGroupHandle`] — a set of outputs sharing workspaces, with its
//!   capabilities and output membership. It carries no workspace content.
//! * [`WorkspaceHandle`] — one workspace, with its name, coordinates, state
//!   bits (active/urgent/hidden) and optional group, plus the `set_*` mutators
//!   the compositor drives it with.
//!
//! A client batches its requests — activate, deactivate, assign, remove, or ask
//! a group to create a workspace — behind `ext_workspace_manager_v1.commit`.
//! wlroots collects them and emits one `commit` signal; this crate copies the
//! batch into owned [`WorkspaceRequest`]s and delivers them to
//! [`ToplevelHandler::workspace_commit`](crate::ToplevelHandler::workspace_commit)
//! through the run's dispatcher. wlroots applies none of them: every one is the
//! compositor's to decide.
//!
//! # Ownership
//!
//! Both handle kinds are owned, and `Drop` is the single release path (there is
//! no public `destroy`). Each watches the manager's `destroy` signal and becomes
//! inert at display teardown, so a handle dropped after the display is a no-op
//! rather than a use-after-free. A handle dropped *inside* a `workspace_commit`
//! delivery is safe without deferral: the crate has already copied the request
//! batch out of wlroots' list, and destroying a workspace or group touches only
//! the manager's object lists, never the `commit` signal wlroots is emitting.

use std::ffi::CString;
use std::ptr::NonNull;

use crate::backend::Registration;
use crate::owned_handle::{OwnedHandle, dangling_usize_test_id, on_watched_destroy};
use crate::runtime::copy_nullable_string;
use crate::{Display, Error, Output, Result, Runtime, sys};

/// Identifies one owned workspace while the compositor holds its handle.
///
/// Opaque to consumers, like [`ForeignToplevelId`](crate::ForeignToplevelId):
/// the compositor receives one from [`WorkspaceHandle::id`] and matches it
/// against the id a commit named. The wrapped value is the handle's own
/// address, which is the identity wlroots' request payloads carry; hiding it
/// keeps that an implementation detail.
///
/// Valid only while a handle is alive: the address may be reused after the handle is dropped.
///
/// Deliberately no `PartialOrd`/`Ord`, matching the other id types in this
/// crate, and `Debug` is redacted because the wrapped value is a heap address.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId(pub(crate) usize);

impl std::fmt::Debug for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkspaceId(..)")
    }
}

impl WorkspaceId {
    /// An id that names no workspace, for negative tests.
    ///
    /// The shared address-space policy
    /// ([`dangling_usize_test_id`](crate::owned_handle::dangling_usize_test_id)):
    /// live values are heap addresses, which never sit at the top of the
    /// address space, so `usize::MAX - n` can never collide with one.
    #[doc(hidden)]
    pub fn dangling_nth_for_test(n: usize) -> Self {
        Self(dangling_usize_test_id(n))
    }
}

/// Identifies one owned workspace group while the compositor holds its handle.
///
/// As [`WorkspaceId`]: opaque, redacted `Debug`, no ordering.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceGroupId(pub(crate) usize);

impl std::fmt::Debug for WorkspaceGroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkspaceGroupId(..)")
    }
}

impl WorkspaceGroupId {
    /// An id that names no group, for negative tests.
    ///
    /// The shared address-space policy
    /// ([`dangling_usize_test_id`](crate::owned_handle::dangling_usize_test_id)):
    /// live values are heap addresses, which never sit at the top of the
    /// address space, so `usize::MAX - n` can never collide with one.
    #[doc(hidden)]
    pub fn dangling_nth_for_test(n: usize) -> Self {
        Self(dangling_usize_test_id(n))
    }
}

/// The capabilities a workspace group advertises to clients.
///
/// A bitmask of `ext_workspace_group_handle_v1_group_capabilities`, hand-rolled
/// rather than a `bitflags` dependency following
/// [`WmCapabilities`](crate::WmCapabilities).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WorkspaceGroupCapabilities(u32);

impl WorkspaceGroupCapabilities {
    /// No capabilities advertised; the initial value and [`Default`].
    pub const NONE: WorkspaceGroupCapabilities = WorkspaceGroupCapabilities(0);
    /// The group accepts `create_workspace`.
    pub const CREATE_WORKSPACE: WorkspaceGroupCapabilities = WorkspaceGroupCapabilities(1);

    /// Whether **every** bit of `other` is advertised here.
    #[must_use]
    pub fn contains(self, other: WorkspaceGroupCapabilities) -> bool {
        self.0 & other.0 == other.0
    }

    /// The raw mask, as the protocol numbers it.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    pub(crate) fn from_raw(raw: u32) -> WorkspaceGroupCapabilities {
        WorkspaceGroupCapabilities(raw)
    }
}

impl std::ops::BitOr for WorkspaceGroupCapabilities {
    type Output = WorkspaceGroupCapabilities;

    fn bitor(self, rhs: WorkspaceGroupCapabilities) -> WorkspaceGroupCapabilities {
        WorkspaceGroupCapabilities(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for WorkspaceGroupCapabilities {
    fn bitor_assign(&mut self, rhs: WorkspaceGroupCapabilities) {
        self.0 |= rhs.0;
    }
}

/// The capabilities a workspace advertises to clients.
///
/// A bitmask of `ext_workspace_handle_v1_workspace_capabilities`, hand-rolled
/// as [`WorkspaceGroupCapabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WorkspaceCapabilities(u32);

impl WorkspaceCapabilities {
    /// No capabilities advertised; the initial value and [`Default`].
    pub const NONE: WorkspaceCapabilities = WorkspaceCapabilities(0);
    /// The workspace accepts `activate`.
    pub const ACTIVATE: WorkspaceCapabilities = WorkspaceCapabilities(1);
    /// The workspace accepts `deactivate`.
    pub const DEACTIVATE: WorkspaceCapabilities = WorkspaceCapabilities(2);
    /// The workspace accepts `remove`.
    pub const REMOVE: WorkspaceCapabilities = WorkspaceCapabilities(4);
    /// The workspace accepts `assign`.
    pub const ASSIGN: WorkspaceCapabilities = WorkspaceCapabilities(8);

    /// Whether **every** bit of `other` is advertised here.
    #[must_use]
    pub fn contains(self, other: WorkspaceCapabilities) -> bool {
        self.0 & other.0 == other.0
    }

    /// The raw mask, as the protocol numbers it.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    pub(crate) fn from_raw(raw: u32) -> WorkspaceCapabilities {
        WorkspaceCapabilities(raw)
    }
}

impl std::ops::BitOr for WorkspaceCapabilities {
    type Output = WorkspaceCapabilities;

    fn bitor(self, rhs: WorkspaceCapabilities) -> WorkspaceCapabilities {
        WorkspaceCapabilities(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for WorkspaceCapabilities {
    fn bitor_assign(&mut self, rhs: WorkspaceCapabilities) {
        self.0 |= rhs.0;
    }
}

/// One request a client batched into a `commit`.
///
/// Copied out of wlroots' request list at emission time — the list and its
/// entries are freed when the commit emission returns, so nothing may be held
/// past the callback. A [`CreateWorkspace`](WorkspaceRequest::CreateWorkspace)
/// names a group, not a workspace, and a `None` group there means the group
/// was destroyed, not that the request was dropped. Every other variant names
/// a workspace; when wlroots NULLed that pointer because the workspace was
/// destroyed before the commit drained, the request is preserved as
/// [`Stale`](WorkspaceRequest::Stale) rather than dropped, so an all-stale
/// batch is distinguishable from an empty commit.
///
/// New-in-milestone and unreleased: marked [`#[non_exhaustive]`] so future
/// request kinds can be added without breaking downstream matches.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkspaceRequest {
    /// A client asked a group to create a workspace. `name` is the requested
    /// name, `None` only if wlroots reported no string; `group` is the group the
    /// request named, `None` if it was destroyed before the commit drained.
    CreateWorkspace {
        /// The requested workspace name.
        name: Option<String>,
        /// The named group, or `None` if it was destroyed before the commit
        /// drained.
        group: Option<WorkspaceGroupId>,
    },
    /// A client asked that a workspace become active.
    Activate(WorkspaceId),
    /// A client asked that a workspace become inactive.
    Deactivate(WorkspaceId),
    /// A client asked that a workspace be moved to a group.
    ///
    /// The `assign` request's group argument is non-nullable, so a client can
    /// never ask to unassign; `group` is `None` here only because wlroots NULLed
    /// it when the group was destroyed before the commit drained.
    Assign {
        /// The workspace being assigned.
        workspace: WorkspaceId,
        /// The named group, or `None` if it was destroyed before the commit
        /// drained.
        group: Option<WorkspaceGroupId>,
    },
    /// A client asked that a workspace be removed.
    Remove(WorkspaceId),
    /// A request that named a workspace destroyed before the commit drained
    /// (wlroots NULLed the pointer), or whose type discriminant is unknown to
    /// this crate. Preserves the request kind plus whatever ids were still
    /// known, so no batch entry is silently lost.
    Stale {
        /// What the dropped entry was asking for.
        kind: StaleRequestKind,
        /// The named workspace, when its pointer was still live. Always `None`
        /// for the NULLed-workspace case that produces this variant today;
        /// `Some` is reserved for future shapes that name more than one object.
        workspace: Option<WorkspaceId>,
        /// The named group, when the request carried one and its pointer was
        /// still live (notably an `Assign` whose workspace died but whose
        /// group survived).
        group: Option<WorkspaceGroupId>,
    },
}

/// Which batch entry a [`WorkspaceRequest::Stale`] preserves.
///
/// The `Unknown` discriminant is the raw `type_` wlroots reported, for request
/// kinds this crate does not know yet.
///
/// New-in-milestone and unreleased: marked [`#[non_exhaustive]`] so future
/// kinds can be added without breaking downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StaleRequestKind {
    /// A NULLed-workspace `activate`.
    Activate,
    /// A NULLed-workspace `deactivate`.
    Deactivate,
    /// A NULLed-workspace `assign` (any surviving group is in `Stale::group`).
    Assign,
    /// A NULLed-workspace `remove`.
    Remove,
    /// A request discriminant this crate does not recognise.
    Unknown(u32),
}

/// The manager-death watch shared by an owned workspace or group handle and its
/// callback.
///
/// [`OwnedHandle`](crate::owned_handle::OwnedHandle) specialised to this
/// module's object: heap-stable, so the listener may name its address for the
/// registration's whole life. See that module for the unlink ordering both
/// paths below preserve.
type HandleListeners<T> = OwnedHandle<T>;

/// A workspace group, owned by the compositor.
///
/// Created by [`Runtime::create_workspace_group`]. Drop is the single release
/// path; the group watches the manager's `destroy` signal and becomes inert at
/// display teardown.
pub struct WorkspaceGroupHandle {
    listeners: Box<HandleListeners<sys::wlr_ext_workspace_group_handle_v1>>,
}

impl std::fmt::Debug for WorkspaceGroupHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceGroupHandle")
            .field("id", &self.id())
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

impl WorkspaceGroupHandle {
    /// Take ownership of a group wlroots just created and link its watch.
    ///
    /// # Safety
    ///
    /// `raw` must be a group returned by
    /// `wlr_ext_workspace_group_handle_v1_create` that has not been destroyed,
    /// with its signals initialised and its manager alive, and the returned
    /// handle must be its only owner.
    pub(crate) unsafe fn from_non_null(
        runtime: Runtime,
        raw: NonNull<sys::wlr_ext_workspace_group_handle_v1>,
    ) -> WorkspaceGroupHandle {
        // SAFETY: forwarded verbatim from this function's contract.
        WorkspaceGroupHandle {
            listeners: unsafe { link_manager_watch(runtime, raw) },
        }
    }

    /// This group's stable identity, safe to store beyond a handler call.
    pub fn id(&self) -> WorkspaceGroupId {
        WorkspaceGroupId(self.listeners.raw.as_ptr() as usize)
    }

    /// Whether the group is still live.
    pub fn is_alive(&self) -> bool {
        self.listeners.is_alive()
    }

    fn raw(&self) -> Option<NonNull<sys::wlr_ext_workspace_group_handle_v1>> {
        self.listeners.live_raw()
    }

    /// The capabilities the compositor advertised for this group.
    ///
    /// [`WorkspaceGroupCapabilities::NONE`] both when nothing was advertised
    /// and when this handle is inert — check [`is_alive`](Self::is_alive) to
    /// tell the two apart.
    #[must_use]
    pub fn capabilities(&self) -> WorkspaceGroupCapabilities {
        let Some(raw) = self.raw() else {
            return WorkspaceGroupCapabilities::NONE;
        };
        // SAFETY: `raw` is live; `caps` is a plain scalar.
        unsafe { WorkspaceGroupCapabilities::from_raw((*raw.as_ptr()).caps) }
    }

    /// Report that this group covers `output`. Idempotent per output, matching
    /// wlroots. `None` for an inert handle.
    pub fn output_enter(&self, output: &Output<'_>) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live and `output`'s handle borrows a live output;
        // wlroots links the pair and sends to clients.
        unsafe {
            sys::wlr_ext_workspace_group_handle_v1_output_enter(raw.as_ptr(), output.as_ptr());
        }
        Some(())
    }

    /// Report that this group no longer covers `output`. `None` for an inert
    /// handle.
    pub fn output_leave(&self, output: &Output<'_>) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: as for `output_enter`.
        unsafe {
            sys::wlr_ext_workspace_group_handle_v1_output_leave(raw.as_ptr(), output.as_ptr());
        }
        Some(())
    }
}

impl Drop for WorkspaceGroupHandle {
    fn drop(&mut self) {
        let Some(raw) = self.listeners.take_live_raw() else {
            return;
        };
        // SAFETY: `take_live_raw` returned `Some`, so the caller's sole-owner
        // contract holds and wlroots frees the group exactly once.
        unsafe { sys::wlr_ext_workspace_group_handle_v1_destroy(raw.as_ptr()) };
    }
}

/// A workspace, owned by the compositor.
///
/// Created by [`Runtime::create_workspace`]. Drop is the single release path;
/// the workspace watches the manager's `destroy` signal and becomes inert at
/// display teardown. Destroying its group does not invalidate this handle —
/// wlroots NULLs the workspace's group pointer from inside the group's own
/// destroy.
pub struct WorkspaceHandle {
    listeners: Box<HandleListeners<sys::wlr_ext_workspace_handle_v1>>,
}

impl std::fmt::Debug for WorkspaceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceHandle")
            .field("id", &self.id())
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

impl WorkspaceHandle {
    /// Take ownership of a workspace wlroots just created and link its watch.
    ///
    /// # Safety
    ///
    /// `raw` must be a workspace returned by `wlr_ext_workspace_handle_v1_create`
    /// that has not been destroyed, with its signals initialised and its manager
    /// alive, and the returned handle must be its only owner.
    pub(crate) unsafe fn from_non_null(
        runtime: Runtime,
        raw: NonNull<sys::wlr_ext_workspace_handle_v1>,
    ) -> WorkspaceHandle {
        // SAFETY: forwarded verbatim from this function's contract.
        WorkspaceHandle {
            listeners: unsafe { link_manager_watch(runtime, raw) },
        }
    }

    /// This workspace's stable identity, safe to store beyond a handler call.
    pub fn id(&self) -> WorkspaceId {
        WorkspaceId(self.listeners.raw.as_ptr() as usize)
    }

    /// Whether the workspace is still live.
    pub fn is_alive(&self) -> bool {
        self.listeners.is_alive()
    }

    fn raw(&self) -> Option<NonNull<sys::wlr_ext_workspace_handle_v1>> {
        self.listeners.live_raw()
    }

    /// The workspace's protocol id string, as the compositor set it at
    /// creation. `None` for an inert handle.
    pub fn id_string(&self) -> Option<String> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live; `id` is a NUL-terminated string wlroots owns,
        // copied out here.
        unsafe { copy_nullable_string((*raw.as_ptr()).id) }
    }

    /// The workspace's display name, `None` until the compositor sets one (or
    /// for an inert handle).
    pub fn name(&self) -> Option<String> {
        let raw = self.raw()?;
        // SAFETY: as for `id_string`.
        unsafe { copy_nullable_string((*raw.as_ptr()).name) }
    }

    /// The workspace's grid coordinates.
    ///
    /// Empty both until the compositor sets any and when this handle is inert —
    /// check [`is_alive`](Self::is_alive) to tell the two apart.
    #[must_use]
    pub fn coordinates(&self) -> Vec<u32> {
        let Some(raw) = self.raw() else {
            return Vec::new();
        };
        // SAFETY: `raw` is live; `coordinates` is a `wl_array` of `u32` whose
        // `data` is either null (empty) or `size / 4` live elements. Copied out.
        unsafe {
            let coords = &(*raw.as_ptr()).coordinates;
            if coords.size == 0 || coords.data.is_null() {
                return Vec::new();
            }
            std::slice::from_raw_parts(coords.data.cast::<u32>(), coords.size / 4).to_vec()
        }
    }

    /// The capabilities the compositor advertised for this workspace.
    ///
    /// [`WorkspaceCapabilities::NONE`] both when nothing was advertised and
    /// when this handle is inert — check [`is_alive`](Self::is_alive) to tell
    /// the two apart.
    #[must_use]
    pub fn capabilities(&self) -> WorkspaceCapabilities {
        let Some(raw) = self.raw() else {
            return WorkspaceCapabilities::NONE;
        };
        // SAFETY: `raw` is live; `caps` is a plain scalar.
        unsafe { WorkspaceCapabilities::from_raw((*raw.as_ptr()).caps) }
    }

    /// The workspace's state bits, read from wlroots' `state` mask.
    fn state_bits(&self) -> Option<u32> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live; `state` is a plain scalar.
        Some(unsafe { (*raw.as_ptr()).state })
    }

    /// `state` mask bit: the compositor reports the workspace active.
    const ACTIVE: u32 = 1;
    /// `state` mask bit: the compositor reports the workspace urgent.
    const URGENT: u32 = 1 << 1;
    /// `state` mask bit: the compositor reports the workspace hidden.
    const HIDDEN: u32 = 1 << 2;

    /// Whether the compositor reports the workspace active.
    ///
    /// `false` both when the bit is clear and when this handle is inert —
    /// check [`is_alive`](Self::is_alive) to tell the two apart.
    #[must_use]
    pub fn active(&self) -> bool {
        self.state_bits()
            .is_some_and(|state| state & Self::ACTIVE != 0)
    }

    /// Whether the compositor reports the workspace urgent.
    ///
    /// `false` both when the bit is clear and when this handle is inert —
    /// check [`is_alive`](Self::is_alive) to tell the two apart.
    #[must_use]
    pub fn urgent(&self) -> bool {
        self.state_bits()
            .is_some_and(|state| state & Self::URGENT != 0)
    }

    /// Whether the compositor reports the workspace hidden.
    ///
    /// `false` both when the bit is clear and when this handle is inert —
    /// check [`is_alive`](Self::is_alive) to tell the two apart.
    #[must_use]
    pub fn hidden(&self) -> bool {
        self.state_bits()
            .is_some_and(|state| state & Self::HIDDEN != 0)
    }

    /// The group this workspace belongs to, if any.
    ///
    /// wlroots NULLs the pointer when the group is destroyed, so this reports
    /// `None` then rather than naming a freed group.
    pub fn group(&self) -> Option<WorkspaceGroupId> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live; `group` is either null or a live group pointer.
        // Only its address is taken, never dereferenced.
        unsafe {
            let group = (*raw.as_ptr()).group;
            NonNull::new(group).map(|group| WorkspaceGroupId(group.as_ptr() as usize))
        }
    }

    /// Report whether the workspace is active. `None` for an inert handle.
    pub fn set_active(&self, enabled: bool) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live; the call only writes a state bit and sends.
        unsafe { sys::wlr_ext_workspace_handle_v1_set_active(raw.as_ptr(), enabled) };
        Some(())
    }

    /// Report whether the workspace is urgent. `None` for an inert handle.
    pub fn set_urgent(&self, enabled: bool) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: as for `set_active`.
        unsafe { sys::wlr_ext_workspace_handle_v1_set_urgent(raw.as_ptr(), enabled) };
        Some(())
    }

    /// Report whether the workspace is hidden. `None` for an inert handle.
    pub fn set_hidden(&self, enabled: bool) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: as for `set_active`.
        unsafe { sys::wlr_ext_workspace_handle_v1_set_hidden(raw.as_ptr(), enabled) };
        Some(())
    }

    /// Set the workspace's display name. `None` for an inert handle or a name
    /// containing an interior NUL, which cannot be passed to wlroots.
    pub fn set_name(&self, name: &str) -> Option<()> {
        let raw = self.raw()?;
        let name = CString::new(name).ok()?;
        // SAFETY: `raw` is live and `name` is a NUL-terminated string wlroots
        // copies into its own storage.
        unsafe { sys::wlr_ext_workspace_handle_v1_set_name(raw.as_ptr(), name.as_ptr()) };
        Some(())
    }

    /// Set the workspace's grid coordinates. An empty slice clears them. `None`
    /// for an inert handle.
    pub fn set_coordinates(&self, coords: &[u32]) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live and `coords` is a live slice of `coords.len()`
        // elements, which wlroots copies into its own `wl_array`.
        unsafe {
            sys::wlr_ext_workspace_handle_v1_set_coordinates(
                raw.as_ptr(),
                coords.as_ptr(),
                coords.len(),
            );
        }
        Some(())
    }

    /// Move the workspace into `group`, or clear its group with `None`. `None`
    /// for an inert handle or an inert group.
    pub fn set_group(&self, group: Option<&WorkspaceGroupHandle>) -> Option<()> {
        let raw = self.raw()?;
        let group = match group {
            Some(group) => group.raw()?.as_ptr(),
            None => std::ptr::null_mut(),
        };
        // SAFETY: `raw` is live and `group` is either null or a live group;
        // wlroots only reads both and sends protocol events.
        unsafe { sys::wlr_ext_workspace_handle_v1_set_group(raw.as_ptr(), group) };
        Some(())
    }
}

impl Drop for WorkspaceHandle {
    fn drop(&mut self) {
        let Some(raw) = self.listeners.take_live_raw() else {
            return;
        };
        // SAFETY: `take_live_raw` returned `Some`, so the caller's sole-owner
        // contract holds and wlroots frees the workspace exactly once.
        unsafe { sys::wlr_ext_workspace_handle_v1_destroy(raw.as_ptr()) };
    }
}

/// Box a fresh listener context and link the manager-death watch on it.
///
/// # Safety
///
/// `raw` must be a live object whose manager is alive and whose signals are
/// initialised, and the returned listeners must be its only owner watch.
unsafe fn link_manager_watch<T>(runtime: Runtime, raw: NonNull<T>) -> Box<HandleListeners<T>> {
    let listeners: Box<HandleListeners<T>> = OwnedHandle::boxed(runtime, raw);
    // The box address is stable from here on, so the watch may name it.
    let session: *const () = OwnedHandle::session(&listeners);

    let manager = listeners
        .runtime
        .ext_workspace_manager_ptr()
        .map(|manager| manager.as_ptr());
    if let Some(manager) = manager {
        // SAFETY: the manager is live and its `destroy` signal is initialised;
        // `listeners` (session and its `alive` cell) outlives this registration.
        let watch = unsafe {
            Registration::link_watched(
                &raw mut (*manager).events.destroy,
                on_watched_destroy::<T>,
                session,
                &listeners.alive,
            )
        };
        *listeners.watched_destroy.borrow_mut() = Some(watch);
    }
    listeners
}

impl Runtime {
    /// Create the `ext_workspace_manager_v1` global. Errors if called twice.
    ///
    /// The manager lives and dies with `display`; this crate never frees it.
    /// Groups and workspaces are created against it with
    /// [`Runtime::create_workspace_group`] and [`Runtime::create_workspace`],
    /// and must be dropped before the display. A client's batched requests reach
    /// [`ToplevelHandler::workspace_commit`](crate::ToplevelHandler::workspace_commit)
    /// once a [`Backend::run_all`](crate::Backend::run_all) has linked the
    /// commit signal — create the manager before the run.
    pub fn create_ext_workspace_manager(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.ext_workspace_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_ext_workspace_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; wlroots owns the returned
        // manager and frees it with the display.
        let raw = unsafe { sys::wlr_ext_workspace_manager_v1_create(display.as_ptr(), version) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_ext_workspace_manager_v1_create"))?;
        *self.inner.ext_workspace_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The manager pointer, once created.
    pub(crate) fn ext_workspace_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_ext_workspace_manager_v1>> {
        *self.inner.ext_workspace_manager.borrow()
    }

    /// Create a workspace group advertising `caps` and return the owned handle.
    ///
    /// `None` when no manager was created, or when wlroots could not allocate
    /// the group. Drop the group before the display.
    pub fn create_workspace_group(
        &self,
        caps: WorkspaceGroupCapabilities,
    ) -> Option<WorkspaceGroupHandle> {
        let manager = self.ext_workspace_manager_ptr()?;
        // SAFETY: `manager` is live and owned by the display; the returned group
        // is freshly allocated and linked into the manager's list.
        let raw =
            unsafe { sys::wlr_ext_workspace_group_handle_v1_create(manager.as_ptr(), caps.bits()) };
        let raw = NonNull::new(raw)?;
        // SAFETY: `raw` is a fresh group with initialised signals and this is
        // its only owner; `from_non_null` links the manager-death watch.
        Some(unsafe { WorkspaceGroupHandle::from_non_null(self.clone(), raw) })
    }

    /// Create a workspace named `id` and advertising `caps`, and return the
    /// owned handle.
    ///
    /// `id` is the stable protocol identifier wlroots copies at creation; a
    /// name is set separately with [`WorkspaceHandle::set_name`]. `None` when no
    /// manager was created, when `id` contains an interior NUL, or when wlroots
    /// could not allocate the workspace. Drop the workspace before the display.
    pub fn create_workspace(
        &self,
        id: &str,
        caps: WorkspaceCapabilities,
    ) -> Option<WorkspaceHandle> {
        let manager = self.ext_workspace_manager_ptr()?;
        let id = CString::new(id).ok()?;
        // SAFETY: `manager` is live and owned by the display, and `id` is a
        // NUL-terminated string wlroots copies; the returned workspace is freshly
        // allocated and linked into the manager's list.
        let raw = unsafe {
            sys::wlr_ext_workspace_handle_v1_create(manager.as_ptr(), id.as_ptr(), caps.bits())
        };
        let raw = NonNull::new(raw)?;
        // SAFETY: `raw` is a fresh workspace with initialised signals and this is
        // its only owner; `from_non_null` links the manager-death watch.
        Some(unsafe { WorkspaceHandle::from_non_null(self.clone(), raw) })
    }
}

/// Copy a client's batched request list into owned [`WorkspaceRequest`]s.
///
/// A request whose workspace pointer wlroots NULLed (the workspace was
/// destroyed before the commit drained) is preserved as
/// [`WorkspaceRequest::Stale`], as is any unknown request discriminant, so an
/// all-stale batch never collapses into an empty commit.
///
/// # Safety
///
/// `head` must be null or an initialised `wl_list` sentinel whose entries are
/// live `wlr_ext_workspace_v1_request`s, and it must be walked only while the
/// commit emission that produced it is still on the stack.
pub(crate) unsafe fn collect_requests(head: *mut sys::wl_list) -> Vec<WorkspaceRequest> {
    if head.is_null() {
        return Vec::new();
    }
    let mut requests = Vec::new();
    // SAFETY: the caller guarantees `head` is an initialised sentinel whose
    // entries are live requests linked through `link`; the walk finishes inside
    // the emission, before wlroots frees them.
    unsafe {
        use sys::wlr_ext_workspace_v1_request_type as RequestType;
        for request in sys::wl_list_for_each!(head, sys::wlr_ext_workspace_v1_request, link) {
            let request = &*request;
            match request.type_ {
                RequestType::WLR_EXT_WORKSPACE_V1_REQUEST_CREATE_WORKSPACE => {
                    let fields = request.__bindgen_anon_1.create_workspace;
                    requests.push(WorkspaceRequest::CreateWorkspace {
                        name: copy_nullable_string(fields.name),
                        group: NonNull::new(fields.group)
                            .map(|group| WorkspaceGroupId(group.as_ptr() as usize)),
                    });
                }
                RequestType::WLR_EXT_WORKSPACE_V1_REQUEST_ACTIVATE => {
                    let raw = request.__bindgen_anon_1.activate.workspace;
                    match workspace_id_of(raw) {
                        Some(id) => requests.push(WorkspaceRequest::Activate(id)),
                        None => requests.push(WorkspaceRequest::Stale {
                            kind: StaleRequestKind::Activate,
                            workspace: None,
                            group: None,
                        }),
                    }
                }
                RequestType::WLR_EXT_WORKSPACE_V1_REQUEST_DEACTIVATE => {
                    let raw = request.__bindgen_anon_1.deactivate.workspace;
                    match workspace_id_of(raw) {
                        Some(id) => requests.push(WorkspaceRequest::Deactivate(id)),
                        None => requests.push(WorkspaceRequest::Stale {
                            kind: StaleRequestKind::Deactivate,
                            workspace: None,
                            group: None,
                        }),
                    }
                }
                RequestType::WLR_EXT_WORKSPACE_V1_REQUEST_ASSIGN => {
                    let fields = request.__bindgen_anon_1.assign;
                    match workspace_id_of(fields.workspace) {
                        Some(workspace) => requests.push(WorkspaceRequest::Assign {
                            workspace,
                            group: NonNull::new(fields.group)
                                .map(|group| WorkspaceGroupId(group.as_ptr() as usize)),
                        }),
                        None => requests.push(WorkspaceRequest::Stale {
                            kind: StaleRequestKind::Assign,
                            workspace: None,
                            group: NonNull::new(fields.group)
                                .map(|group| WorkspaceGroupId(group.as_ptr() as usize)),
                        }),
                    }
                }
                RequestType::WLR_EXT_WORKSPACE_V1_REQUEST_REMOVE => {
                    let raw = request.__bindgen_anon_1.remove.workspace;
                    match workspace_id_of(raw) {
                        Some(id) => requests.push(WorkspaceRequest::Remove(id)),
                        None => requests.push(WorkspaceRequest::Stale {
                            kind: StaleRequestKind::Remove,
                            workspace: None,
                            group: None,
                        }),
                    }
                }
                unknown => requests.push(WorkspaceRequest::Stale {
                    kind: StaleRequestKind::Unknown(unknown.0),
                    workspace: None,
                    group: None,
                }),
            }
        }
    }
    requests
}

/// Wrap a request's workspace pointer, `None` when wlroots NULLed it because the
/// workspace was destroyed before the commit drained.
fn workspace_id_of(raw: *mut sys::wlr_ext_workspace_handle_v1) -> Option<WorkspaceId> {
    NonNull::new(raw).map(|raw| WorkspaceId(raw.as_ptr() as usize))
}
