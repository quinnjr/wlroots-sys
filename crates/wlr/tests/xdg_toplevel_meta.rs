//! The M9 xdg-toplevel-icon and xdg-toplevel-tag wrappers against a real
//! (headless) compositor.
//!
//! The manager create/double-create contract is exercised without a client;
//! the event paths are client-driven through the shared harness, which speaks
//! the wire protocol with `wayland-client`. A real client sets an icon (stock
//! name plus a pixel buffer) and a tag/description, and the server-side handler
//! records what arrived.

mod common;

use std::thread::JoinHandle;

use wlr::{Backend, Display, Runtime, Toplevel, ToplevelIcon, Until};

/// Both manager globals create once and refuse a second create, and the
/// size-preference call is safe before and after the manager exists.
#[test]
fn icon_and_tag_managers_create_once() {
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    // The size call is a harmless no-op before any manager exists.
    runtime.set_toplevel_icon_sizes(&[]);

    runtime
        .create_xdg_toplevel_icon_manager(&display, 1)
        .expect("icon manager");
    assert!(
        runtime
            .create_xdg_toplevel_icon_manager(&display, 1)
            .is_err(),
        "a second icon manager is refused"
    );
    // Empty and non-empty preferences are both accepted; wlroots copies the
    // slice, so a short-lived borrow is enough.
    runtime.set_toplevel_icon_sizes(&[]);
    runtime.set_toplevel_icon_sizes(&[16, 32, 64]);

    runtime
        .create_xdg_toplevel_tag_manager(&display, 1)
        .expect("tag manager");
    assert!(
        runtime
            .create_xdg_toplevel_tag_manager(&display, 1)
            .is_err(),
        "a second tag manager is refused"
    );
}

/// Records the icon and tag/description the server-side handler observes.
struct App {
    client: Option<JoinHandle<common::client::ClientEvents>>,
    /// `Some(..)` once the icon event fired; the inner value is the icon's
    /// `name()`, so `Some(None)` means an icon with no name arrived.
    icon_name: Option<Option<String>>,
    /// Whether the delivered icon carried a pixel buffer.
    icon_has_buffer: Option<bool>,
    /// Whether a clone of the delivered icon was independent (the refcount
    /// path a compositor keeping two references would use).
    icon_clone_ok: Option<bool>,
    tag: Option<Option<String>>,
    description: Option<Option<String>>,
}

impl wlr::OutputHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::SeatHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

impl wlr::ToplevelHandler for App {
    fn toplevel_icon_changed(&mut self, _toplevel: &Toplevel<'_>, icon: Option<ToplevelIcon>) {
        self.icon_name = Some(icon.as_ref().and_then(ToplevelIcon::name));
        self.icon_has_buffer = Some(icon.as_ref().is_some_and(|icon| icon.buffer().is_some()));
        // Take a second reference and drop the first; both must remain valid.
        if let Some(icon) = icon {
            let cloned = icon.clone();
            self.icon_clone_ok = Some(cloned.name() == icon.name());
        }
    }

    fn toplevel_tag_changed(&mut self, _toplevel: &Toplevel<'_>, tag: Option<&str>) {
        self.tag = Some(tag.map(str::to_owned));
    }

    fn toplevel_description_changed(
        &mut self,
        _toplevel: &Toplevel<'_>,
        description: Option<&str>,
    ) {
        self.description = Some(description.map(str::to_owned));
    }
}

/// A client sets an icon (name + buffer) and a tag/description on a live
/// toplevel; the owned [`ToplevelIcon`] and both strings reach the handler.
#[test]
fn client_drives_icon_and_tag_events() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime
        .create_xdg_toplevel_icon_manager(&display, 1)
        .expect("icon manager");
    runtime
        .create_xdg_toplevel_tag_manager(&display, 1)
        .expect("tag manager");
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        client: Some(common::client::spawn_toplevel_meta(&socket)),
        icon_name: None,
        icon_has_buffer: None,
        icon_clone_ok: None,
        tag: None,
        description: None,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    let _events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert_eq!(
        app.icon_name,
        Some(Some("wlr-test-icon".to_owned())),
        "the icon's stock name reached the handler"
    );
    assert_eq!(
        app.icon_has_buffer,
        Some(true),
        "the icon's pixel buffer reached the handler"
    );
    assert_eq!(
        app.icon_clone_ok,
        Some(true),
        "a clone of the owned icon names the same icon"
    );
    assert_eq!(
        app.tag,
        Some(Some("wlr-test-tag".to_owned())),
        "the tag reached the handler"
    );
    assert_eq!(
        app.description,
        Some(Some("WlR test description".to_owned())),
        "the description reached the handler"
    );
}
