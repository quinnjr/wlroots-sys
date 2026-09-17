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
//! # Privileged: this global broadcasts workspace layout to whoever binds it
//!
//! Every client that binds the manager receives every group and workspace —
//! names, grid coordinates, and the active/urgent/hidden bits — and keeps
//! receiving updates for as long as it stays bound. That is a
//! layout-surveillance primitive readable by any process that can connect:
//! which workspaces exist, what they are named, where they sit on the grid,
//! which one is active. The other direction is just as open: a bound client
//! may ask to activate, deactivate, remove or reassign any workspace, or ask
//! a group to create one, and those requests land on the defaulted
//! [`ToplevelHandler::workspace_commit`](crate::ToplevelHandler::workspace_commit)
//! with no client identity attached — wlroots applies none of them, but it
//! also stops none of them, so every one is the compositor's to allow or
//! refuse, exactly like `SeatHandler::request_activate`.
//!
//! Gate the manager global at bind time with a
//! [`Runtime::lookup_security_context`](crate::Runtime::lookup_security_context)-based
//! filter on the display and default-deny: allow only clients whose context
//! you trust (a known pager, say), deny the rest. The commit handler is
//! defaulted, and the default ignores every request — keep that posture
//! until a request arrives from a client you vetted at bind time.
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

define_capability_mask! {
    /// The capabilities a workspace group advertises to clients.
    ///
    /// A bitmask of `ext_workspace_group_handle_v1_group_capabilities`,
    /// hand-rolled rather than a `bitflags` dependency, and generated by the
    /// shared `define_capability_mask!` core in the private `capability_mask`
    /// module together with [`WmCapabilities`](crate::WmCapabilities) and
    /// [`WorkspaceCapabilities`], so the three spellings of this shape cannot
    /// drift apart.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct WorkspaceGroupCapabilities(u32);
    /// No capabilities advertised; the initial value and [`Default`].
    pub const NONE = 0,
    /// The group accepts `create_workspace`.
    pub const CREATE_WORKSPACE = 1,
}

define_capability_mask! {
    /// The capabilities a workspace advertises to clients.
    ///
    /// A bitmask of `ext_workspace_handle_v1_workspace_capabilities`,
    /// hand-rolled rather than a `bitflags` dependency, and generated by the
    /// shared `define_capability_mask!` core in the private `capability_mask`
    /// module together with [`WmCapabilities`](crate::WmCapabilities) and
    /// [`WorkspaceGroupCapabilities`], so the three spellings of this shape
    /// cannot drift apart.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct WorkspaceCapabilities(u32);
    /// No capabilities advertised; the initial value and [`Default`].
    pub const NONE = 0,
    /// The workspace accepts `activate`.
    pub const ACTIVATE = 1,
    /// The workspace accepts `deactivate`.
    pub const DEACTIVATE = 2,
    /// The workspace accepts `remove`.
    pub const REMOVE = 4,
    /// The workspace accepts `assign`.
    pub const ASSIGN = 8,
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
    /// check [`is_alive`](Self::is_alive) to tell the two apart. A stored
    /// size that is not a multiple of `u32` — which wlroots never produces,
    /// since it only ever writes whole coordinates — is treated as corrupt
    /// and also reads back as empty rather than truncating to a prefix.
    #[must_use]
    pub fn coordinates(&self) -> Vec<u32> {
        let Some(raw) = self.raw() else {
            return Vec::new();
        };
        // SAFETY: `raw` is live; `coordinates` is a `wl_array` of `u32` whose
        // `data` is either null (empty) or `size / size_of::<u32>()` live
        // elements. Copied out. The corrupt-size arm below returns before any
        // read, so the slice is only ever built over whole elements.
        unsafe {
            let coords = &(*raw.as_ptr()).coordinates;
            if coords.size == 0 || coords.data.is_null() {
                return Vec::new();
            }
            let elem = std::mem::size_of::<u32>();
            debug_assert_eq!(
                coords.size % elem,
                0,
                "wlr_ext_workspace coordinates array size is not a multiple of u32"
            );
            if coords.size % elem != 0 {
                return Vec::new();
            }
            std::slice::from_raw_parts(coords.data.cast::<u32>(), coords.size / elem).to_vec()
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

    /// Move the workspace into `group`, or clear its group with `None`.
    ///
    /// Grouping is manager-scoped: `group` must come from the same
    /// [`Runtime`] (and therefore the same manager) as `self`. A group from
    /// another runtime is refused with `None` — wlroots would otherwise link
    /// two handles whose managers die independently, leaving the workspace
    /// naming freed memory once the group's manager is torn down while this
    /// handle stays live. Runtime identity is compared with `Rc::ptr_eq` on
    /// the two handles' stored runtimes, so clones of one [`Runtime`] assign
    /// freely.
    ///
    /// `None` for an inert handle (either side), for a cross-runtime group,
    /// or when `group` names an inert handle.
    pub fn set_group(&self, group: Option<&WorkspaceGroupHandle>) -> Option<()> {
        let raw = self.raw()?;
        let group = match group {
            Some(group) => {
                if !std::rc::Rc::ptr_eq(
                    &self.listeners.runtime.inner,
                    &group.listeners.runtime.inner,
                ) {
                    // A caller mistake with a defined miss contract, like an
                    // interior-NUL name: silent `None` rather than a
                    // `debug_assert`, so debug builds (this crate's own test
                    // suite included) can pin the refusal without trapping.
                    return None;
                }
                group.raw()?.as_ptr()
            }
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
/// A missing manager here is a caller-contract violation, not a runtime
/// condition: every production caller reaches this through
/// [`Runtime::create_workspace_group`] or [`Runtime::create_workspace`],
/// both of which miss when no manager was ever created, so a live manager
/// always exists to watch. Asserted in debug like the other
/// should-never-fire arms in this crate; release keeps the previous behavior
/// (a watch-less handle) unchanged.
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
    debug_assert!(
        manager.is_some(),
        "ext_workspace handle created with no manager to watch"
    );
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
    ///
    /// The teardown watch linked here clears the stored pointer when the
    /// display dies, so the double-create guard resets with it: a fresh
    /// display can install a new manager on the same runtime afterwards.
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
        // Link the teardown watch before returning: the manager dies with the
        // display, and without this the stored pointer would dangle across
        // display teardown. The watch clears the pointer and the liveness
        // flag from inside the manager's own `destroy` emission, while the
        // manager memory is still valid.
        self.inner.ext_workspace_manager_alive.set(true);
        // SAFETY: `raw` is a live manager with initialised signals, and
        // `self.inner` (the session pointer and the `alive` cell) outlives
        // the registration: both live in the same `Rc`-allocated
        // `RuntimeInner`, whose heap address never moves, and the callback
        // unlinks itself before either can go stale. `link_watched`'s
        // contract otherwise forwarded verbatim.
        let watch = unsafe {
            Registration::link_watched(
                &raw mut (*raw.as_ptr()).events.destroy,
                on_ext_workspace_manager_destroy,
                std::rc::Rc::as_ptr(&self.inner).cast::<()>(),
                &self.inner.ext_workspace_manager_alive as *const _,
            )
        };
        *self.inner.ext_workspace_manager_destroy.borrow_mut() = Some(watch);
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
    /// `None` for three distinct misses, all silent by design (there is no
    /// [`Error`](crate::Error) variant that names one without stretching its
    /// documented semantics, and this signature is frozen within the 0.20.x
    /// line): no manager was ever created; the manager died with its display
    /// (the teardown watch linked at creation cleared the stored pointer, so a
    /// post-teardown call misses rather than dereferencing freed memory); or
    /// wlroots returned null for a live manager, which is an allocation
    /// failure the test suite asserts never happens
    /// (`debug_assert!(false, ...)` fires there, with no release behavior
    /// change).
    ///
    /// Drop the group before the display: see [`WorkspaceGroupHandle`].
    pub fn create_workspace_group(
        &self,
        caps: WorkspaceGroupCapabilities,
    ) -> Option<WorkspaceGroupHandle> {
        if !self.inner.ext_workspace_manager_alive.get() {
            return None;
        }
        let manager = self.ext_workspace_manager_ptr()?;
        // SAFETY: the liveness flag above is true, so the manager's `destroy`
        // has not fired and the display-owned `manager` is still live; the
        // returned group is freshly allocated and linked into its list.
        let raw =
            unsafe { sys::wlr_ext_workspace_group_handle_v1_create(manager.as_ptr(), caps.bits()) };
        let raw = match NonNull::new(raw) {
            Some(raw) => raw,
            // Unexpected: the manager is live, so only an allocation failure
            // explains a null return. Asserted in debug like the other
            // should-never-fire arms in this crate; still a silent miss,
            // never trapped.
            None => {
                debug_assert!(
                    false,
                    "wlr_ext_workspace_group_handle_v1_create returned null with a live manager"
                );
                return None;
            }
        };
        // SAFETY: `raw` is a fresh group with initialised signals and this is
        // its only owner; `from_non_null` links the manager-death watch.
        Some(unsafe { WorkspaceGroupHandle::from_non_null(self.clone(), raw) })
    }

    /// Create a workspace named `id` and advertising `caps`, and return the
    /// owned handle.
    ///
    /// `id` is the stable protocol identifier wlroots copies at creation; a
    /// name is set separately with [`WorkspaceHandle::set_name`].
    ///
    /// `None` for four distinct misses, all silent by design (there is no
    /// [`Error`](crate::Error) variant that names one without stretching its
    /// documented semantics, and this signature is frozen within the 0.20.x
    /// line): no manager was ever created; the manager died with its display
    /// (the teardown watch linked at creation cleared the stored pointer, so a
    /// post-teardown call misses rather than dereferencing freed memory);
    /// `id` contains an interior NUL, which cannot be passed to wlroots and is
    /// refused rather than truncated; or wlroots returned null for a live
    /// manager, which is an allocation failure the test suite asserts never
    /// happens (`debug_assert!(false, ...)` fires there, with no release
    /// behavior change).
    ///
    /// Drop the workspace before the display: see [`WorkspaceHandle`].
    pub fn create_workspace(
        &self,
        id: &str,
        caps: WorkspaceCapabilities,
    ) -> Option<WorkspaceHandle> {
        if !self.inner.ext_workspace_manager_alive.get() {
            return None;
        }
        let manager = self.ext_workspace_manager_ptr()?;
        let id = CString::new(id).ok()?;
        // SAFETY: the liveness flag above is true, so the manager's `destroy`
        // has not fired and the display-owned `manager` is still live, and
        // `id` is a NUL-terminated string wlroots copies; the returned
        // workspace is freshly allocated and linked into its list.
        let raw = unsafe {
            sys::wlr_ext_workspace_handle_v1_create(manager.as_ptr(), id.as_ptr(), caps.bits())
        };
        let raw = match NonNull::new(raw) {
            Some(raw) => raw,
            // Unexpected: the manager is live, so only an allocation failure
            // explains a null return. Asserted in debug like the other
            // should-never-fire arms in this crate; still a silent miss,
            // never trapped.
            None => {
                debug_assert!(
                    false,
                    "wlr_ext_workspace_handle_v1_create returned null with a live manager"
                );
                return None;
            }
        };
        // SAFETY: `raw` is a fresh workspace with initialised signals and this is
        // its only owner; `from_non_null` links the manager-death watch.
        Some(unsafe { WorkspaceHandle::from_non_null(self.clone(), raw) })
    }
}

/// The ext-workspace manager is being destroyed (display teardown, before
/// the manager itself is freed).
///
/// Clears the stored manager pointer and the liveness flag while the manager
/// memory is still valid, so a later [`Runtime::create_workspace_group`] or
/// [`Runtime::create_workspace`] returns `None` instead of dereferencing
/// freed memory, and unlinks this listener so wlroots' post-destroy
/// empty-signal assertion holds. Clearing the pointer also resets the
/// double-create guard, so a fresh display can install a new manager on the
/// same runtime afterwards. A later `RuntimeInner` drop then finds the flag
/// false and the registration gone, and never touches the freed signal list.
///
/// This is the manager side of the teardown story; each owned
/// [`WorkspaceGroupHandle`]'s and [`WorkspaceHandle`]'s own watch (the shared
/// owned-handle manager-death watch) marks that handle inert at the same
/// emission.
unsafe extern "C" fn on_ext_workspace_manager_destroy(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `create_ext_workspace_manager` into a live
    // manager's `events.destroy` with a `session` pointing at the owning
    // `RuntimeInner`, which outlives the registration. Every call below is
    // infallible and cannot unwind out of this `extern "C"` frame.
    unsafe {
        let session = crate::backend::bound_session(l);
        if session.is_null() {
            return;
        }
        let inner = &*session.cast::<crate::runtime::RuntimeInner>();
        inner.ext_workspace_manager_alive.set(false);
        *inner.ext_workspace_manager.borrow_mut() = None;
        crate::backend::remove_listener(l);
        let registration = inner.ext_workspace_manager_destroy.borrow_mut().take();
        drop(registration);
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
                        group: group_id_of(fields.group),
                    });
                }
                RequestType::WLR_EXT_WORKSPACE_V1_REQUEST_ACTIVATE => {
                    requests.push(stale_or_live(
                        request.__bindgen_anon_1.activate.workspace,
                        StaleRequestKind::Activate,
                        WorkspaceRequest::Activate,
                    ));
                }
                RequestType::WLR_EXT_WORKSPACE_V1_REQUEST_DEACTIVATE => {
                    requests.push(stale_or_live(
                        request.__bindgen_anon_1.deactivate.workspace,
                        StaleRequestKind::Deactivate,
                        WorkspaceRequest::Deactivate,
                    ));
                }
                RequestType::WLR_EXT_WORKSPACE_V1_REQUEST_ASSIGN => {
                    let fields = request.__bindgen_anon_1.assign;
                    match workspace_id_of(fields.workspace) {
                        Some(workspace) => requests.push(WorkspaceRequest::Assign {
                            workspace,
                            group: group_id_of(fields.group),
                        }),
                        None => requests.push(WorkspaceRequest::Stale {
                            kind: StaleRequestKind::Assign,
                            workspace: None,
                            group: group_id_of(fields.group),
                        }),
                    }
                }
                RequestType::WLR_EXT_WORKSPACE_V1_REQUEST_REMOVE => {
                    requests.push(stale_or_live(
                        request.__bindgen_anon_1.remove.workspace,
                        StaleRequestKind::Remove,
                        WorkspaceRequest::Remove,
                    ));
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

/// Wrap a request's group pointer, `None` when wlroots NULLed it because the
/// group was destroyed before the commit drained.
fn group_id_of(raw: *mut sys::wlr_ext_workspace_group_handle_v1) -> Option<WorkspaceGroupId> {
    NonNull::new(raw).map(|raw| WorkspaceGroupId(raw.as_ptr() as usize))
}

/// Map a workspace-only request through `live`, preserving a NULLed
/// (destroyed-before-drain) workspace as [`WorkspaceRequest::Stale`] of
/// `kind` instead of dropping the entry — so an all-stale batch never
/// collapses into an empty commit.
///
/// `Assign` does not go through here: it carries a group alongside the
/// workspace, and the surviving group is kept on its `Stale` entry.
fn stale_or_live(
    raw: *mut sys::wlr_ext_workspace_handle_v1,
    kind: StaleRequestKind,
    live: impl FnOnce(WorkspaceId) -> WorkspaceRequest,
) -> WorkspaceRequest {
    match workspace_id_of(raw) {
        Some(id) => live(id),
        None => WorkspaceRequest::Stale {
            kind,
            workspace: None,
            group: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The group capability bits are the ones `ext-workspace-v1` declares.
    /// Pinning them against the generated client bindings is what makes the
    /// hand-rolled bitmask a checked decision: a protocol renumbering changes
    /// the generated values and this test fails rather than every
    /// advertisement being wrong in silence.
    #[test]
    fn group_capability_bits_are_the_ones_the_protocol_declares() {
        use wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::GroupCapabilities as C;
        assert_eq!(
            WorkspaceGroupCapabilities::CREATE_WORKSPACE.bits(),
            u32::from(C::CreateWorkspace)
        );
        let only = WorkspaceGroupCapabilities::CREATE_WORKSPACE;
        assert!(only.contains(WorkspaceGroupCapabilities::CREATE_WORKSPACE));
        assert!(!WorkspaceGroupCapabilities::NONE.contains(only));
    }

    /// `Default` is the empty mask, and `BitOrAssign` accumulates exactly like
    /// `BitOr`: the two spellings must agree, or a caller mixing them silently
    /// advertises different capabilities.
    #[test]
    fn group_capabilities_default_is_none_and_assign_accumulates() {
        assert_eq!(
            WorkspaceGroupCapabilities::default(),
            WorkspaceGroupCapabilities::NONE
        );
        assert_eq!(WorkspaceGroupCapabilities::NONE.bits(), 0);
        let mut caps = WorkspaceGroupCapabilities::default();
        caps |= WorkspaceGroupCapabilities::CREATE_WORKSPACE;
        assert_eq!(caps, WorkspaceGroupCapabilities::CREATE_WORKSPACE);
        assert!(caps.contains(WorkspaceGroupCapabilities::CREATE_WORKSPACE));
    }

    /// The workspace capability bits are the ones `ext-workspace-v1`
    /// declares, pinned the same way as the group bits above.
    #[test]
    fn workspace_capability_bits_are_the_ones_the_protocol_declares() {
        use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::WorkspaceCapabilities as C;
        assert_eq!(
            WorkspaceCapabilities::ACTIVATE.bits(),
            u32::from(C::Activate)
        );
        assert_eq!(
            WorkspaceCapabilities::DEACTIVATE.bits(),
            u32::from(C::Deactivate)
        );
        assert_eq!(WorkspaceCapabilities::REMOVE.bits(), u32::from(C::Remove));
        assert_eq!(WorkspaceCapabilities::ASSIGN.bits(), u32::from(C::Assign));
        assert_eq!(
            (WorkspaceCapabilities::ACTIVATE | WorkspaceCapabilities::ASSIGN).bits(),
            u32::from(C::Activate) | u32::from(C::Assign)
        );
        let both = WorkspaceCapabilities::ACTIVATE | WorkspaceCapabilities::ASSIGN;
        assert!(both.contains(WorkspaceCapabilities::ACTIVATE));
        assert!(both.contains(WorkspaceCapabilities::ASSIGN));
        assert!(!WorkspaceCapabilities::ACTIVATE.contains(WorkspaceCapabilities::ASSIGN));
    }

    /// `Default` is the empty mask, and `BitOrAssign` accumulates exactly like
    /// `BitOr`, as for the group capabilities above.
    #[test]
    fn workspace_capabilities_default_is_none_and_assign_accumulates() {
        assert_eq!(
            WorkspaceCapabilities::default(),
            WorkspaceCapabilities::NONE
        );
        assert_eq!(WorkspaceCapabilities::NONE.bits(), 0);
        let mut caps = WorkspaceCapabilities::default();
        caps |= WorkspaceCapabilities::ACTIVATE;
        assert_eq!(caps, WorkspaceCapabilities::ACTIVATE);
        caps |= WorkspaceCapabilities::REMOVE;
        assert_eq!(
            caps,
            WorkspaceCapabilities::ACTIVATE | WorkspaceCapabilities::REMOVE
        );
        assert!(caps.contains(WorkspaceCapabilities::ACTIVATE | WorkspaceCapabilities::REMOVE));
    }

    /// Drain one synthetic request through [`collect_requests`].
    ///
    /// Links `request` into a stack sentinel and walks it while both are
    /// live — the synchronous, no-event-loop analog of the commit emission
    /// `collect_requests` is written for. The caller owns whatever payload it
    /// planted in `request`.
    ///
    /// # Safety
    ///
    /// `request` must be a live, exclusively-owned allocation that outlives
    /// the call, with its `type_` set and, for a discriminant this test reads
    /// a payload for, that payload valid for the call.
    unsafe fn drain_one(request: *mut sys::wlr_ext_workspace_v1_request) -> Vec<WorkspaceRequest> {
        // SAFETY: the caller guarantees `request` is live and exclusively
        // owned; the hand-wired links form a valid single-entry list whose
        // head and entry both outlive the synchronous walk.
        unsafe {
            let mut head: sys::wl_list = std::mem::zeroed();
            head.prev = &raw mut (*request).link;
            head.next = &raw mut (*request).link;
            (*request).link.prev = &raw mut head;
            (*request).link.next = &raw mut head;
            collect_requests(&raw mut head)
        }
    }

    /// A request discriminant this crate does not recognise is preserved as
    /// `Stale`, never silently dropped.
    #[test]
    fn unknown_request_discriminant_arrives_stale() {
        // SAFETY: `request` is a stack allocation that outlives the
        // synchronous drain; only `type_` is read for this discriminant, and
        // the union stays zeroed.
        unsafe {
            let mut request: sys::wlr_ext_workspace_v1_request = std::mem::zeroed();
            request.type_ = sys::wlr_ext_workspace_v1_request_type(u32::MAX);
            assert_eq!(
                drain_one(&raw mut request),
                vec![WorkspaceRequest::Stale {
                    kind: StaleRequestKind::Unknown(u32::MAX),
                    workspace: None,
                    group: None,
                }],
                "an unrecognised discriminant is preserved, never dropped"
            );
        }
    }

    /// A `create_workspace` whose group was destroyed before the commit
    /// drained still arrives — with `group: None` rather than dropped — so
    /// the batch keeps its shape.
    #[test]
    fn create_workspace_with_destroyed_group_arrives_with_none_group() {
        let name = CString::new("wlr-new").expect("test name");
        // SAFETY: `request` is a stack allocation that outlives the
        // synchronous drain; `name` outlives it too, and
        // `copy_nullable_string` copies the string out rather than holding it.
        unsafe {
            let mut request: sys::wlr_ext_workspace_v1_request = std::mem::zeroed();
            request.type_ = sys::wlr_ext_workspace_v1_request_type::WLR_EXT_WORKSPACE_V1_REQUEST_CREATE_WORKSPACE;
            request.__bindgen_anon_1 = sys::wlr_ext_workspace_v1_request__bindgen_ty_1 {
                create_workspace: sys::wlr_ext_workspace_v1_request__bindgen_ty_1__bindgen_ty_1 {
                    name: name.as_ptr().cast_mut(),
                    group: std::ptr::null_mut(),
                },
            };
            assert_eq!(
                drain_one(&raw mut request),
                vec![WorkspaceRequest::CreateWorkspace {
                    name: Some("wlr-new".to_owned()),
                    group: None,
                }],
                "the create survived its group's destruction with a None group"
            );
        }
    }

    /// An inert group ignores `output_enter`/`output_leave` without touching
    /// the output: both miss before the output pointer is ever read.
    #[test]
    fn inert_group_ignores_output_enter_leave_without_touching_the_output() {
        use std::alloc::{Layout, alloc_zeroed, dealloc};

        let _guard = crate::test_support::test_display_guard();
        crate::interest::tests::headless_env();
        let display = Display::new().expect("display");
        let runtime = Runtime::new().expect("runtime");
        runtime
            .create_ext_workspace_manager(&display, 1)
            .expect("manager");
        let group = runtime
            .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
            .expect("group");

        // Destroy the display out from under the group. wlroots emits the
        // manager's `destroy` while its memory is still valid, and both the
        // manager watch and the group's own watch run there.
        drop(display);
        assert!(!group.is_alive(), "the manager's death was observed");

        // A scratch output the test never initialises as one: sound only
        // because the assertions below return before dereferencing it — the
        // inert miss fires on the group's own liveness first. `alloc_zeroed`
        // rather than `mem::zeroed` for the reason `ScratchSurface` documents:
        // `wlr_output` embeds `wl_signal` machinery with bare function
        // pointers.
        let layout = Layout::new::<sys::wlr_output>();
        // SAFETY: `wlr_output` is non-zero-sized, so `alloc_zeroed` returns
        // either null (checked below) or a suitably aligned, zeroed
        // allocation of exactly that size.
        let raw = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_output>();
        assert!(!raw.is_null(), "allocation failed");
        // SAFETY: the group is inert, so both calls below return `None`
        // before reading `output`; the scratch pointer is never
        // dereferenced, and the allocation outlives both calls.
        unsafe {
            let output = crate::Output::from_raw(raw);
            assert_eq!(
                group.output_enter(&output),
                None,
                "no enter writes against a freed manager"
            );
            assert_eq!(group.output_leave(&output), None, "no leave writes either");
            // SAFETY: `raw` was allocated by `alloc_zeroed` with the matching
            // layout, is still exclusively owned, and nothing else frees it.
            dealloc(raw.cast::<u8>(), layout);
        }
        drop(group);
    }
}
