//! `ext-foreign-toplevel-list` and `ext-workspace`: the two `ext` shell
//! families M9 completes.
//!
//! `ext-foreign-toplevel-list` is observation-only for the compositor: it
//! exports an owned [`ExtForeignToplevelHandle`] and updates its state. The
//! client-driven leg binds `ext_foreign_toplevel_list_v1` (from
//! `wayland-protocols`' `staging` set, already a dev-dependency) and observes
//! the replay.
//!
//! `ext-workspace` is bidirectional: the compositor owns the group and
//! workspace objects, and clients batch activate/deactivate/assign/remove/
//! create requests behind a `commit`. Both legs are exercised, including a
//! real client commit that lands in [`wlr::ToplevelHandler::workspace_commit`].

mod common;

use std::thread::JoinHandle;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1, ext_workspace_handle_v1, ext_workspace_manager_v1,
};
use wlr::{
    Backend, Display, ExtForeignToplevelState, Runtime, StaleRequestKind, ToplevelHandler, Until,
    WorkspaceCapabilities, WorkspaceGroupCapabilities, WorkspaceGroupHandle, WorkspaceRequest,
};

/// Build an [`ExtForeignToplevelState`] without a struct literal: the type is
/// `#[non_exhaustive]`, so integration tests (a downstream crate) cannot use
/// a struct expression at all — not even with `..Default::default()`.
/// Field assignment on a `default()` value is the supported construction.
fn foreign_state(title: Option<&str>, app_id: Option<&str>) -> ExtForeignToplevelState {
    let mut state = ExtForeignToplevelState::default();
    state.title = title.map(str::to_owned);
    state.app_id = app_id.map(str::to_owned);
    state
}

/// The simplest handler that runs a client thread to completion, for the
/// observation-only foreign-toplevel-list leg.
#[derive(Default)]
struct Idle {
    client: Option<JoinHandle<common::client::ExtForeignToplevelEvents>>,
}

impl wlr::OutputHandler for Idle {}
impl wlr::ToplevelHandler for Idle {}
impl wlr::SeatHandler for Idle {}
impl wlr::FdHandler for Idle {}

impl wlr::LoopHandler for Idle {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// The handler the workspace commit leg records into.
#[derive(Default)]
struct WorkspaceApp {
    client: Option<JoinHandle<common::client::ExtWorkspaceEvents>>,
    requests: Vec<WorkspaceRequest>,
}

impl wlr::OutputHandler for WorkspaceApp {}
impl wlr::SeatHandler for WorkspaceApp {}
impl wlr::FdHandler for WorkspaceApp {}

impl ToplevelHandler for WorkspaceApp {
    fn workspace_commit(&mut self, requests: &[WorkspaceRequest]) {
        self.requests.extend_from_slice(requests);
    }
}

impl wlr::LoopHandler for WorkspaceApp {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

// ---------------------------------------------------------------------------
// ext-foreign-toplevel-list
// ---------------------------------------------------------------------------

/// The list global creates once and refuses a second create, and the handle
/// factory reports the missing-list miss rather than a wrong default.
#[test]
fn ext_foreign_toplevel_list_creates_once_and_handles_need_it() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    let state = foreign_state(Some("Wlr Test Window"), Some("org.wlr.test"));
    assert!(
        runtime.create_ext_foreign_toplevel(&state).is_none(),
        "no list, no handle"
    );

    runtime
        .create_ext_foreign_toplevel_list(&display, 1)
        .expect("list");
    assert!(
        runtime
            .create_ext_foreign_toplevel_list(&display, 1)
            .is_err(),
        "a second list is refused"
    );
}

/// The handle's state round-trips through `update_state`, and an interior NUL
/// is refused rather than truncated.
#[test]
fn ext_foreign_toplevel_handle_state_round_trips() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");
    runtime
        .create_ext_foreign_toplevel_list(&display, 1)
        .expect("list");

    let handle = runtime
        .create_ext_foreign_toplevel(&foreign_state(Some("First"), Some("org.wlr.ext")))
        .expect("a fresh handle");
    assert!(handle.is_alive());
    assert_eq!(handle.state().title.as_deref(), Some("First"));
    assert_eq!(handle.state().app_id.as_deref(), Some("org.wlr.ext"));
    assert!(
        handle.identifier().is_some(),
        "wlroots mints a stable identifier"
    );

    handle
        .update_state(&foreign_state(Some("Second"), None))
        .expect("update");
    assert_eq!(handle.state().title.as_deref(), Some("Second"));
    assert_eq!(handle.state().app_id, None);

    // An interior NUL is refused rather than truncated.
    assert_eq!(
        handle.update_state(&foreign_state(Some("bad\0title"), None)),
        None,
        "a title with an interior NUL is refused"
    );
    assert_eq!(handle.state().title.as_deref(), Some("Second"));

    // The app-id field refuses the same way, leaving the previous state in
    // place.
    let before = handle.state();
    assert_eq!(
        handle.update_state(&foreign_state(None, Some("bad\0id"))),
        None,
        "an app id with an interior NUL is refused"
    );
    assert_eq!(
        handle.state(),
        before,
        "a refused update leaves the previous state in place"
    );

    // Creation refuses a NUL title the same way the update path does.
    assert!(
        runtime
            .create_ext_foreign_toplevel(&foreign_state(Some("a\0b"), None))
            .is_none(),
        "a create with an interior-NUL title is refused"
    );
    assert!(
        runtime
            .create_ext_foreign_toplevel(&foreign_state(None, Some("a\0b")))
            .is_none(),
        "a create with an interior-NUL app id is refused"
    );
}

/// Two independent handles drop safely in either order; neither release path
/// touches the other.
#[test]
fn ext_foreign_toplevel_handle_destroy_order_is_safe() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");
    runtime
        .create_ext_foreign_toplevel_list(&display, 1)
        .expect("list");

    for first_first in [true, false] {
        let first = runtime
            .create_ext_foreign_toplevel(&foreign_state(Some("first"), None))
            .expect("first");
        let second = runtime
            .create_ext_foreign_toplevel(&foreign_state(Some("second"), None))
            .expect("second");
        if first_first {
            drop(first);
            assert!(second.is_alive());
            drop(second);
        } else {
            drop(second);
            assert!(first.is_alive());
            drop(first);
        }
    }
}

/// Dropping the display first makes the handle inert: accessors miss and `Drop`
/// does not touch the freed list.
#[test]
fn ext_foreign_toplevel_display_death_makes_handle_inert() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");
    runtime
        .create_ext_foreign_toplevel_list(&display, 1)
        .expect("list");
    let handle = runtime
        .create_ext_foreign_toplevel(&foreign_state(Some("live"), None))
        .expect("handle");
    assert!(handle.is_alive());
    assert_eq!(handle.state().title.as_deref(), Some("live"));

    drop(display);

    assert!(!handle.is_alive(), "the list's death was observed");
    assert_eq!(handle.state(), ExtForeignToplevelState::default());
    assert_eq!(
        handle.identifier(),
        None,
        "no accessor dereferences freed memory"
    );
    assert_eq!(
        handle.update_state(&ExtForeignToplevelState::default()),
        None,
        "no mutator writes either"
    );
    assert!(
        runtime
            .create_ext_foreign_toplevel(&foreign_state(Some("late"), None))
            .is_none(),
        "the list's death cleared the stored pointer, so a post-teardown \
         create misses instead of dereferencing freed memory"
    );
    drop(handle);
}

/// A real `ext_foreign_toplevel_list_v1` client binds the list and observes the
/// handle the compositor exported, with the state `update_state` last set.
#[test]
fn a_client_observes_an_ext_foreign_toplevel() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_ext_foreign_toplevel_list(&display, 1)
        .expect("list");

    let handle = runtime
        .create_ext_foreign_toplevel(&foreign_state(Some("Initial"), Some("org.wlr.initial")))
        .expect("handle");
    // The client must observe the *updated* values, which only `update_state`
    // could have produced.
    handle
        .update_state(&foreign_state(
            Some("Wlr Test Window"),
            Some("org.wlr.test"),
        ))
        .expect("update");

    let socket = display.add_socket_auto().expect("socket");
    let mut app = Idle {
        client: Some(common::client::spawn_ext_foreign_toplevel(&socket)),
    };

    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(
        events.toplevels_seen, 1,
        "the client saw one exported handle"
    );
    assert_eq!(events.title.as_deref(), Some("Wlr Test Window"));
    assert_eq!(events.app_id.as_deref(), Some("org.wlr.test"));
    assert!(events.saw_done, "the initial done event arrived");
    assert!(
        events.identifier.is_some(),
        "the stable identifier event arrived"
    );
    assert!(
        !events.saw_closed,
        "the observed handle is never closed during the run, so no request \
         is driven against it post-close"
    );
    drop(handle);
}

// ---------------------------------------------------------------------------
// ext-workspace
// ---------------------------------------------------------------------------

/// The manager global creates once and refuses a second create, and the group
/// and workspace factories miss until it exists.
#[test]
fn workspace_manager_creates_once_and_handles_need_it() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    assert!(
        runtime
            .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
            .is_none(),
        "no manager, no group"
    );
    assert!(
        runtime
            .create_workspace("1", WorkspaceCapabilities::ACTIVATE)
            .is_none(),
        "no manager, no workspace"
    );

    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");
    assert!(
        runtime.create_ext_workspace_manager(&display, 1).is_err(),
        "a second manager is refused"
    );

    // An interior NUL cannot reach wlroots: the id is refused, not truncated.
    assert!(
        runtime
            .create_workspace("bad\0id", WorkspaceCapabilities::ACTIVATE)
            .is_none(),
        "a workspace id with an interior NUL is refused"
    );
}

/// Every workspace mutator round-trips through its accessor, and group
/// assignment tracks the handle.
#[test]
fn workspace_mutators_round_trip() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");

    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");
    assert_eq!(
        group.capabilities(),
        WorkspaceGroupCapabilities::CREATE_WORKSPACE
    );

    let caps = WorkspaceCapabilities::ACTIVATE
        | WorkspaceCapabilities::DEACTIVATE
        | WorkspaceCapabilities::ASSIGN
        | WorkspaceCapabilities::REMOVE;
    let workspace = runtime.create_workspace("1", caps).expect("workspace");
    assert!(workspace.is_alive());
    assert_eq!(
        workspace.id_string().as_deref(),
        Some("1"),
        "the protocol id string is what creation was given"
    );
    assert_eq!(workspace.capabilities(), caps);
    assert_eq!(workspace.name(), None);
    assert_eq!(workspace.coordinates(), Vec::<u32>::new());
    assert!(!workspace.active() && !workspace.urgent() && !workspace.hidden());
    assert_eq!(workspace.group(), None);

    workspace.set_name("Main").expect("name");
    assert_eq!(workspace.name().as_deref(), Some("Main"));

    workspace.set_coordinates(&[3, 4]).expect("coordinates");
    assert_eq!(workspace.coordinates(), vec![3, 4]);
    // An empty slice clears the coordinates rather than leaving them in
    // place.
    workspace.set_coordinates(&[]).expect("clear coordinates");
    assert_eq!(
        workspace.coordinates(),
        Vec::<u32>::new(),
        "clearing the coordinates reads back as empty"
    );

    workspace.set_active(true).expect("active");
    workspace.set_urgent(true).expect("urgent");
    workspace.set_hidden(true).expect("hidden");
    assert!(workspace.active() && workspace.urgent() && workspace.hidden());

    workspace.set_active(false).expect("deactivate");
    workspace.set_urgent(false).expect("calm");
    workspace.set_hidden(false).expect("show");
    assert!(!workspace.active() && !workspace.urgent() && !workspace.hidden());

    workspace.set_group(Some(&group)).expect("assign");
    assert_eq!(workspace.group(), Some(group.id()));
    workspace.set_group(None).expect("unassign");
    assert_eq!(workspace.group(), None);

    // An interior NUL is refused rather than truncated.
    assert_eq!(workspace.set_name("bad\0name"), None);
    assert_eq!(workspace.name().as_deref(), Some("Main"));
}

/// Dropping the group before the workspace is safe in both directions: wlroots
/// NULLs the workspace's group pointer from inside the group's own destroy.
#[test]
fn workspace_group_destroy_order_is_safe() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");

    for group_first in [true, false] {
        let group = runtime
            .create_workspace_group(WorkspaceGroupCapabilities::NONE)
            .expect("group");
        let workspace = runtime
            .create_workspace(
                "1",
                WorkspaceCapabilities::ACTIVATE | WorkspaceCapabilities::ASSIGN,
            )
            .expect("workspace");
        workspace.set_group(Some(&group)).expect("assign");
        assert_eq!(workspace.group(), Some(group.id()));

        if group_first {
            drop(group);
            assert!(workspace.is_alive(), "the workspace survives its group");
            assert_eq!(
                workspace.group(),
                None,
                "wlroots NULLs the workspace's group pointer on group destroy"
            );
            workspace.set_group(None).expect("clear");
            drop(workspace);
        } else {
            drop(workspace);
            assert!(group.is_alive());
            drop(group);
        }
    }
}

/// Dropping the display first makes both handle kinds inert.
#[test]
fn workspace_display_death_makes_handles_inert() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");
    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");
    let workspace = runtime
        .create_workspace("1", WorkspaceCapabilities::ACTIVATE)
        .expect("workspace");
    workspace.set_group(Some(&group)).expect("assign");

    drop(display);

    assert!(!group.is_alive(), "the manager's death was observed");
    assert!(!workspace.is_alive());
    assert_eq!(group.capabilities(), WorkspaceGroupCapabilities::NONE);
    assert_eq!(workspace.id_string(), None, "the id string misses too");
    assert_eq!(workspace.name(), None);
    assert_eq!(
        workspace.coordinates(),
        Vec::<u32>::new(),
        "the coordinates miss too"
    );
    assert_eq!(
        workspace.capabilities(),
        WorkspaceCapabilities::NONE,
        "the capabilities miss too"
    );
    assert_eq!(workspace.group(), None);
    assert!(!workspace.active());
    assert!(!workspace.urgent(), "the urgent bit misses too");
    assert!(!workspace.hidden(), "the hidden bit misses too");
    assert_eq!(workspace.set_name("late"), None);
    assert_eq!(workspace.set_active(true), None);
    assert_eq!(workspace.set_urgent(true), None);
    assert_eq!(workspace.set_hidden(true), None);
    assert_eq!(
        workspace.set_coordinates(&[1, 2]),
        None,
        "no coordinate writes either"
    );
    drop(workspace);
    drop(group);
}

/// Dropping the display clears the stored manager pointer, so a later create
/// misses instead of dereferencing freed memory — and the double-create guard
/// resets with it, so a fresh display can install a new manager on the same
/// runtime.
#[test]
fn workspace_manager_teardown_clears_and_recreates() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");
    assert!(
        runtime
            .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
            .is_some(),
        "the live manager creates groups"
    );

    drop(display);

    assert!(
        runtime
            .create_workspace_group(WorkspaceGroupCapabilities::NONE)
            .is_none(),
        "the manager's death cleared the stored pointer, so a post-teardown \
         group create misses instead of dereferencing freed memory"
    );
    assert!(
        runtime
            .create_workspace("late", WorkspaceCapabilities::ACTIVATE)
            .is_none(),
        "a post-teardown workspace create misses the same way"
    );

    // The cleared pointer resets the double-create guard: a fresh display
    // installs a new manager, and it creates usable handles.
    let display = Display::new().expect("fresh display");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("a fresh display installs a new manager");
    assert!(
        runtime.create_ext_workspace_manager(&display, 1).is_err(),
        "the guard is armed again once a manager exists"
    );
    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("the new manager creates groups");
    let workspace = runtime
        .create_workspace("1", WorkspaceCapabilities::ACTIVATE)
        .expect("the new manager creates workspaces");
    workspace.set_group(Some(&group)).expect("assign");
    assert_eq!(workspace.group(), Some(group.id()));
    drop((workspace, group));
}

/// `set_group` refuses an inert or foreign group: grouping is
/// manager-scoped, and a dead manager's pointer must never be touched.
#[test]
fn set_group_refuses_inert_and_foreign_groups() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");
    let old_group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");
    let old_workspace = runtime
        .create_workspace("old", WorkspaceCapabilities::ASSIGN)
        .expect("workspace");

    // Kill the manager out from under both handles.
    drop(display);
    assert!(!old_group.is_alive() && !old_workspace.is_alive());

    // A fresh display installs a new manager on the same runtime, with live
    // handles of its own.
    let display = Display::new().expect("fresh display");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("fresh manager");
    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");
    let workspace = runtime
        .create_workspace("1", WorkspaceCapabilities::ASSIGN)
        .expect("workspace");

    // The control: two live handles from the same manager assign freely.
    workspace.set_group(Some(&group)).expect("assign");
    assert_eq!(workspace.group(), Some(group.id()));

    // A live workspace against an inert group refuses — the group's own
    // liveness fails before its freed manager is ever touched.
    assert_eq!(
        workspace.set_group(Some(&old_group)),
        None,
        "a group from the dead manager is refused"
    );
    assert_eq!(
        workspace.group(),
        Some(group.id()),
        "the refused assign left the previous group in place"
    );
    // An inert workspace refuses everything, including clearing.
    assert_eq!(
        old_workspace.set_group(Some(&group)),
        None,
        "an inert workspace cannot be assigned"
    );
    assert_eq!(
        old_workspace.set_group(Some(&old_group)),
        None,
        "two inert handles refuse each other"
    );
    assert_eq!(
        old_workspace.set_group(None),
        None,
        "even clearing the group is refused once the workspace is inert"
    );
    // A group from another runtime is foreign even when both sides are live.
    let foreign_runtime = Runtime::new().expect("foreign runtime");
    let foreign_display = Display::new().expect("foreign display");
    foreign_runtime
        .create_ext_workspace_manager(&foreign_display, 1)
        .expect("foreign manager");
    let foreign_group = foreign_runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("foreign group");
    assert_eq!(
        workspace.set_group(Some(&foreign_group)),
        None,
        "a group from another runtime is refused"
    );
    assert_eq!(
        workspace.group(),
        Some(group.id()),
        "the refused foreign assign left the previous group in place"
    );
    drop((workspace, group, old_workspace, old_group));
    drop((foreign_group, foreign_display, foreign_runtime));
}

/// `output_enter`/`output_leave` are safe against a live output, and the
/// round trip leaves the group usable.
#[test]
fn workspace_group_output_enter_leave_are_safe() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");
    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");

    struct Probe {
        group: Option<WorkspaceGroupHandle>,
        seen: bool,
    }
    impl wlr::OutputHandler for Probe {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            if let Some(group) = &self.group {
                assert!(
                    group.output_enter(output).is_some(),
                    "output_enter reports the live group"
                );
                assert!(
                    group.output_leave(output).is_some(),
                    "output_leave reports the live group"
                );
                self.seen = true;
            }
        }
    }
    impl wlr::ToplevelHandler for Probe {}
    impl wlr::SeatHandler for Probe {}
    impl wlr::FdHandler for Probe {}
    impl wlr::LoopHandler for Probe {}

    let mut probe = Probe {
        group: Some(group),
        seen: false,
    };
    backend
        .run_all(&display, &mut probe, &runtime, Until::Turns(4))
        .expect("run_all");
    assert!(probe.seen, "the live output reached output_enter/leave");
    assert!(probe.group.as_ref().expect("group").is_alive());
}

/// A real `ext_workspace_manager_v1` client observes the replayed group and
/// workspace, then commits every request the protocol carries — each landing in
/// [`ToplevelHandler::workspace_commit`] as one atomic batch.
#[test]
fn a_client_commits_workspace_requests() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");

    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");
    let workspace = runtime
        .create_workspace(
            "1",
            WorkspaceCapabilities::ACTIVATE
                | WorkspaceCapabilities::DEACTIVATE
                | WorkspaceCapabilities::ASSIGN
                | WorkspaceCapabilities::REMOVE,
        )
        .expect("workspace");
    workspace.set_name("Main").expect("name");
    workspace.set_group(Some(&group)).expect("assign");
    let workspace_id = workspace.id();
    let group_id = group.id();

    let socket = display.add_socket_auto().expect("socket");
    let mut app = WorkspaceApp {
        client: Some(common::client::spawn_ext_workspace(&socket)),
        requests: Vec::new(),
    };

    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(events.groups_seen, 1, "the client saw one group");
    assert_eq!(events.workspaces_seen, 1, "the client saw one workspace");
    assert!(events.saw_done, "the initial done event arrived");
    assert!(
        !events.saw_finished,
        "the manager was never finished while requests were committed against it"
    );
    assert!(
        !events.group_removed,
        "the group was never removed while the client drove requests against it"
    );
    assert!(
        !events.workspace_removed,
        "the workspace was never removed while the client drove requests against it"
    );

    assert_eq!(
        app.requests,
        vec![
            WorkspaceRequest::Activate(workspace_id),
            WorkspaceRequest::Deactivate(workspace_id),
            WorkspaceRequest::Assign {
                workspace: workspace_id,
                group: Some(group_id),
            },
            WorkspaceRequest::CreateWorkspace {
                name: Some("wlr-new".to_owned()),
                group: Some(group_id),
            },
            WorkspaceRequest::Remove(workspace_id),
        ],
        "every committed request reached the handler in order"
    );
    drop((workspace, group));
}

// ---------------------------------------------------------------------------
// `collect_requests` NULL branches: destroyed targets are preserved, not lost
// ---------------------------------------------------------------------------

/// Test-local client state for the destroy-before-commit legs: the shared
/// `spawn_ext_workspace` driver commits one fixed batch, so this file owns
/// the variants that stage a request, wait for the server to destroy its
/// target, and only then commit.
#[derive(Default)]
struct StagedState {
    group: Option<ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1>,
    workspace: Option<ext_workspace_handle_v1::ExtWorkspaceHandleV1>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for StagedState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_workspace_manager_v1::ExtWorkspaceManagerV1, ()> for StagedState {
    fn event(
        state: &mut Self,
        _proxy: &ext_workspace_manager_v1::ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state.group = Some(workspace_group);
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.workspace = Some(workspace);
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(StagedState, ext_workspace_manager_v1::ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (
            ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
            ()
        ),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (
            ext_workspace_handle_v1::ExtWorkspaceHandleV1,
            ()
        ),
    ]);
}

impl Dispatch<ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1, ()> for StagedState {
    fn event(
        _state: &mut Self,
        _proxy: &ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
        _event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_workspace_handle_v1::ExtWorkspaceHandleV1, ()> for StagedState {
    fn event(
        _state: &mut Self,
        _proxy: &ext_workspace_handle_v1::ExtWorkspaceHandleV1,
        _event: ext_workspace_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// Which target the staged leg destroys before the commit drains.
#[derive(Clone, Copy)]
enum StagedDestroy {
    Group,
    Workspace,
}

/// Which single request the staged leg stages before the commit drains.
#[derive(Clone, Copy)]
enum StagedRequest {
    Assign,
    Activate,
    Deactivate,
    Remove,
}

/// Stage one request, wait for the server to destroy `destroy`'s target,
/// then commit. The commit — not the staging — is what the server drains, so
/// wlroots has already NULLed the destroyed target when `collect_requests`
/// walks the batch.
fn spawn_staged_request(
    socket: &str,
    staged: std::sync::mpsc::Sender<()>,
    destroyed: std::sync::mpsc::Receiver<()>,
    request: StagedRequest,
) -> JoinHandle<()> {
    let path = common::isolated_runtime_dir().join(socket);
    let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to wayland socket");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("set read timeout on wayland socket");
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("set write timeout on wayland socket");
    std::thread::spawn(move || {
        let conn = Connection::from_socket(stream).expect("wrap wayland socket");
        let (globals, mut queue) =
            registry_queue_init::<StagedState>(&conn).expect("registry queue init");
        let qh = queue.handle();
        let mut state = StagedState::default();

        let manager: ext_workspace_manager_v1::ExtWorkspaceManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind ext_workspace_manager_v1");
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the replayed objects are dispatched");
        let group = state.group.clone().expect("the manager replayed a group");
        let workspace = state
            .workspace
            .clone()
            .expect("the manager replayed a workspace");

        // Stage but do not commit: the request is flushed so the server
        // receives it while the target is still live, and the commit that
        // drains it only follows once the server has destroyed the target.
        // The flush comes before the channel send because only socket
        // traffic wakes the server's blocking dispatch: a channel send
        // alone would leave the server asleep while this thread blocks.
        match request {
            StagedRequest::Assign => workspace.assign(&group),
            StagedRequest::Activate => workspace.activate(),
            StagedRequest::Deactivate => workspace.deactivate(),
            StagedRequest::Remove => workspace.remove(),
        }
        conn.flush().expect("flush the staged assign");
        staged.send(()).expect("stage the assign");
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server dispatches the staged assign");
        destroyed
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the server destroyed the target");
        manager.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server sees the commit");
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server drains");

        drop((workspace, group, manager));
    })
}

/// The server side of the staged legs: records the commit like
/// [`WorkspaceApp`], and destroys one held handle once the client has staged
/// its request but before the commit arrives.
struct StagedApp {
    client: Option<JoinHandle<()>>,
    requests: Vec<WorkspaceRequest>,
    staged: std::sync::mpsc::Receiver<()>,
    destroyed: Option<std::sync::mpsc::Sender<()>>,
    group: Option<WorkspaceGroupHandle>,
    workspace: Option<wlr::WorkspaceHandle>,
    destroy: StagedDestroy,
}

impl wlr::OutputHandler for StagedApp {}
impl wlr::SeatHandler for StagedApp {}
impl wlr::FdHandler for StagedApp {}

impl ToplevelHandler for StagedApp {
    fn workspace_commit(&mut self, requests: &[WorkspaceRequest]) {
        self.requests.extend_from_slice(requests);
    }
}

impl wlr::LoopHandler for StagedApp {
    fn should_stop(&mut self) -> bool {
        // The client staged its request: destroy the target now, so the
        // NULLing happens before the commit drains, then let the client go.
        if self.destroyed.is_some() && self.staged.try_recv().is_ok() {
            match self.destroy {
                StagedDestroy::Group => drop(self.group.take()),
                StagedDestroy::Workspace => drop(self.workspace.take()),
            }
            if let Some(destroyed) = self.destroyed.take() {
                let _ = destroyed.send(());
            }
        }
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// An `assign` whose group the server destroyed after staging still arrives —
/// with `group: None` rather than dropped — so the batch keeps its shape.
#[test]
fn assign_with_destroyed_group_arrives_with_none_group() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");

    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");
    let workspace = runtime
        .create_workspace(
            "1",
            WorkspaceCapabilities::ACTIVATE
                | WorkspaceCapabilities::DEACTIVATE
                | WorkspaceCapabilities::ASSIGN
                | WorkspaceCapabilities::REMOVE,
        )
        .expect("workspace");
    let workspace_id = workspace.id();

    let socket = display.add_socket_auto().expect("socket");
    let (staged_tx, staged_rx) = std::sync::mpsc::channel();
    let (destroyed_tx, destroyed_rx) = std::sync::mpsc::channel();
    let mut app = StagedApp {
        client: Some(spawn_staged_request(
            &socket,
            staged_tx,
            destroyed_rx,
            StagedRequest::Assign,
        )),
        requests: Vec::new(),
        staged: staged_rx,
        destroyed: Some(destroyed_tx),
        group: Some(group),
        workspace: Some(workspace),
        destroy: StagedDestroy::Group,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(
        app.requests,
        vec![WorkspaceRequest::Assign {
            workspace: workspace_id,
            group: None,
        }],
        "the assign survived its group's destruction with a None group"
    );
}

/// An `assign` whose workspace the server destroyed after staging is
/// preserved as `Stale` — never silently dropped, so an all-stale batch is
/// distinguishable from an empty commit.
#[test]
fn assign_with_destroyed_workspace_arrives_stale() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");

    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");
    let workspace = runtime
        .create_workspace(
            "1",
            WorkspaceCapabilities::ACTIVATE
                | WorkspaceCapabilities::DEACTIVATE
                | WorkspaceCapabilities::ASSIGN
                | WorkspaceCapabilities::REMOVE,
        )
        .expect("workspace");
    let group_id = group.id();

    let socket = display.add_socket_auto().expect("socket");
    let (staged_tx, staged_rx) = std::sync::mpsc::channel();
    let (destroyed_tx, destroyed_rx) = std::sync::mpsc::channel();
    let mut app = StagedApp {
        client: Some(spawn_staged_request(
            &socket,
            staged_tx,
            destroyed_rx,
            StagedRequest::Assign,
        )),
        requests: Vec::new(),
        staged: staged_rx,
        destroyed: Some(destroyed_tx),
        group: Some(group),
        workspace: Some(workspace),
        destroy: StagedDestroy::Workspace,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    // The kind is matched exactly — the workspace/group payloads are asserted
    // by value alongside it.
    assert_eq!(
        app.requests.len(),
        1,
        "the destroyed-workspace assign still arrived exactly once: {:?}",
        app.requests
    );
    match &app.requests[0] {
        WorkspaceRequest::Stale {
            kind,
            workspace,
            group,
        } => {
            assert_eq!(
                *kind,
                StaleRequestKind::Assign,
                "the stale entry names the assign kind"
            );
            assert_eq!(*workspace, None, "the destroyed workspace names no id");
            assert_eq!(
                *group,
                Some(group_id),
                "the surviving group is kept on the stale entry"
            );
        }
        other => panic!(
            "the assign survived its workspace's destruction as Stale, keeping the live group: {other:?}"
        ),
    }
}

/// Stage one workspace-only request, have the server destroy the workspace
/// before the commit drains, and return the delivered batch.
fn staged_workspace_destroy_delivers(request: StagedRequest) -> Vec<WorkspaceRequest> {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");

    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");
    let workspace = runtime
        .create_workspace(
            "1",
            WorkspaceCapabilities::ACTIVATE
                | WorkspaceCapabilities::DEACTIVATE
                | WorkspaceCapabilities::ASSIGN
                | WorkspaceCapabilities::REMOVE,
        )
        .expect("workspace");

    let socket = display.add_socket_auto().expect("socket");
    let (staged_tx, staged_rx) = std::sync::mpsc::channel();
    let (destroyed_tx, destroyed_rx) = std::sync::mpsc::channel();
    let mut app = StagedApp {
        client: Some(spawn_staged_request(
            &socket,
            staged_tx,
            destroyed_rx,
            request,
        )),
        requests: Vec::new(),
        staged: staged_rx,
        destroyed: Some(destroyed_tx),
        group: Some(group),
        workspace: Some(workspace),
        destroy: StagedDestroy::Workspace,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");
    app.requests
}

/// An `activate` whose workspace the server destroyed after staging is
/// preserved as `Stale` with exactly the activate kind — never silently
/// dropped, so an all-stale batch is distinguishable from an empty commit.
#[test]
fn staged_activate_with_destroyed_workspace_arrives_stale() {
    assert_eq!(
        staged_workspace_destroy_delivers(StagedRequest::Activate),
        vec![WorkspaceRequest::Stale {
            kind: StaleRequestKind::Activate,
            workspace: None,
            group: None,
        }],
        "the activate survived its workspace's destruction as Stale"
    );
}

/// A `deactivate` whose workspace the server destroyed after staging is
/// preserved as `Stale` with exactly the deactivate kind.
#[test]
fn staged_deactivate_with_destroyed_workspace_arrives_stale() {
    assert_eq!(
        staged_workspace_destroy_delivers(StagedRequest::Deactivate),
        vec![WorkspaceRequest::Stale {
            kind: StaleRequestKind::Deactivate,
            workspace: None,
            group: None,
        }],
        "the deactivate survived its workspace's destruction as Stale"
    );
}

/// A `remove` whose workspace the server destroyed after staging is preserved
/// as `Stale` with exactly the remove kind.
#[test]
fn staged_remove_with_destroyed_workspace_arrives_stale() {
    assert_eq!(
        staged_workspace_destroy_delivers(StagedRequest::Remove),
        vec![WorkspaceRequest::Stale {
            kind: StaleRequestKind::Remove,
            workspace: None,
            group: None,
        }],
        "the remove survived its workspace's destruction as Stale"
    );
}

/// The handler the empty-commit leg records into: one entry per commit, so an
/// empty commit is visible instead of vanishing into a flat request list.
struct CommitCountingApp {
    client: Option<JoinHandle<common::client::ExtWorkspaceEvents>>,
    commits: Vec<Vec<WorkspaceRequest>>,
}

impl wlr::OutputHandler for CommitCountingApp {}
impl wlr::SeatHandler for CommitCountingApp {}
impl wlr::FdHandler for CommitCountingApp {}

impl ToplevelHandler for CommitCountingApp {
    fn workspace_commit(&mut self, requests: &[WorkspaceRequest]) {
        self.commits.push(requests.to_vec());
    }
}

impl wlr::LoopHandler for CommitCountingApp {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

/// Bind the manager and commit nothing: the server still emits one commit —
/// an empty batch delivered exactly once — so an all-stale batch (one `Stale`
/// entry) is distinguishable from an empty one (no entries).
#[test]
fn empty_commit_delivers_one_empty_batch() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_ext_workspace_manager(&display, 1)
        .expect("manager");

    let group = runtime
        .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
        .expect("group");
    let workspace = runtime
        .create_workspace(
            "1",
            WorkspaceCapabilities::ACTIVATE
                | WorkspaceCapabilities::DEACTIVATE
                | WorkspaceCapabilities::ASSIGN
                | WorkspaceCapabilities::REMOVE,
        )
        .expect("workspace");

    let socket = display.add_socket_auto().expect("socket");
    let mut app = CommitCountingApp {
        client: Some(common::client::spawn_empty_commit(&socket)),
        commits: Vec::new(),
    };

    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(events.groups_seen, 1, "the client bound and saw the group");
    assert_eq!(
        events.workspaces_seen, 1,
        "the client bound and saw the workspace"
    );
    assert!(events.saw_done, "the initial done event arrived");
    assert_eq!(
        app.commits,
        vec![Vec::new()],
        "the empty commit was delivered exactly once, as an empty batch"
    );
    drop((workspace, group));
}
