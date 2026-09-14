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

use wlr::{
    Backend, Display, ExtForeignToplevelState, Runtime, ToplevelHandler, Until,
    WorkspaceCapabilities, WorkspaceGroupCapabilities, WorkspaceGroupHandle, WorkspaceRequest,
};

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

    let state = ExtForeignToplevelState {
        title: Some("Wlr Test Window".to_owned()),
        app_id: Some("org.wlr.test".to_owned()),
    };
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
        .create_ext_foreign_toplevel(&ExtForeignToplevelState {
            title: Some("First".to_owned()),
            app_id: Some("org.wlr.ext".to_owned()),
        })
        .expect("a fresh handle");
    assert!(handle.is_alive());
    assert_eq!(handle.state().title.as_deref(), Some("First"));
    assert_eq!(handle.state().app_id.as_deref(), Some("org.wlr.ext"));
    assert!(
        handle.identifier().is_some(),
        "wlroots mints a stable identifier"
    );

    handle
        .update_state(&ExtForeignToplevelState {
            title: Some("Second".to_owned()),
            app_id: None,
        })
        .expect("update");
    assert_eq!(handle.state().title.as_deref(), Some("Second"));
    assert_eq!(handle.state().app_id, None);

    // An interior NUL is refused rather than truncated.
    assert_eq!(
        handle.update_state(&ExtForeignToplevelState {
            title: Some("bad\0title".to_owned()),
            app_id: None,
        }),
        None,
        "a title with an interior NUL is refused"
    );
    assert_eq!(handle.state().title.as_deref(), Some("Second"));
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
            .create_ext_foreign_toplevel(&ExtForeignToplevelState {
                title: Some("first".to_owned()),
                app_id: None,
            })
            .expect("first");
        let second = runtime
            .create_ext_foreign_toplevel(&ExtForeignToplevelState {
                title: Some("second".to_owned()),
                app_id: None,
            })
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
        .create_ext_foreign_toplevel(&ExtForeignToplevelState {
            title: Some("live".to_owned()),
            app_id: None,
        })
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
        .create_ext_foreign_toplevel(&ExtForeignToplevelState {
            title: Some("Initial".to_owned()),
            app_id: Some("org.wlr.initial".to_owned()),
        })
        .expect("handle");
    // The client must observe the *updated* values, which only `update_state`
    // could have produced.
    handle
        .update_state(&ExtForeignToplevelState {
            title: Some("Wlr Test Window".to_owned()),
            app_id: Some("org.wlr.test".to_owned()),
        })
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
    assert_eq!(workspace.capabilities(), caps);
    assert_eq!(workspace.name(), None);
    assert_eq!(workspace.coordinates(), Vec::<u32>::new());
    assert!(!workspace.active() && !workspace.urgent() && !workspace.hidden());
    assert_eq!(workspace.group(), None);

    workspace.set_name("Main").expect("name");
    assert_eq!(workspace.name().as_deref(), Some("Main"));

    workspace.set_coordinates(&[3, 4]).expect("coordinates");
    assert_eq!(workspace.coordinates(), vec![3, 4]);

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
    assert_eq!(workspace.name(), None);
    assert_eq!(workspace.group(), None);
    assert!(!workspace.active());
    assert_eq!(workspace.set_name("late"), None);
    assert_eq!(workspace.set_active(true), None);
    drop(workspace);
    drop(group);
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
                group.output_enter(output);
                group.output_leave(output);
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
