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

use common::client::IconForm;
use wlr::{Backend, Display, Error, Runtime, Toplevel, ToplevelIcon, Until};

/// Both manager globals create once and refuse a second create; sizing
/// without a manager is refused rather than silently dropped, and sizing
/// after the create is accepted.
#[test]
fn icon_and_tag_managers_create_once() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let runtime = Runtime::new().expect("runtime");

    // Sizing a global that does not exist is a caller bug, not a default.
    assert!(
        matches!(
            runtime.set_toplevel_icon_sizes(&[]),
            Err(Error::Operation(_))
        ),
        "sizing with no manager is refused"
    );

    runtime
        .create_xdg_toplevel_icon_manager(&display, 1)
        .expect("icon manager");
    assert!(
        matches!(
            runtime.create_xdg_toplevel_icon_manager(&display, 1),
            Err(Error::Operation(_))
        ),
        "a second icon manager is refused as a double-create"
    );
    // Empty and non-empty preferences are both accepted; wlroots copies the
    // slice, so a short-lived borrow is enough.
    runtime.set_toplevel_icon_sizes(&[]).expect("empty sizes");
    runtime
        .set_toplevel_icon_sizes(&[16, 32, 64])
        .expect("sized preferences");

    runtime
        .create_xdg_toplevel_tag_manager(&display, 1)
        .expect("tag manager");
    assert!(
        matches!(
            runtime.create_xdg_toplevel_tag_manager(&display, 1),
            Err(Error::Operation(_))
        ),
        "a second tag manager is refused as a double-create"
    );
}

/// Records the icon and tag/description the server-side handler observes.
///
/// The set deliveries are read off the arrival sequences' first entries, so
/// the resets and empty values the driver sends afterwards cannot clobber
/// them; the buffer and clone probes stay scalar because only `Some`
/// deliveries carry an icon to inspect.
struct App {
    client: Option<JoinHandle<common::client::ClientEvents>>,
    /// Whether the first delivered icon carried a pixel buffer.
    icon_has_buffer: Option<bool>,
    /// Whether a clone of the delivered icon was independent (the refcount
    /// path a compositor keeping two references would use).
    icon_clone_ok: Option<bool>,
    /// Every icon delivery in arrival order (`None` = the reset): the set is
    /// asserted off the first entry, the clear off the last.
    icon_sequence: Vec<Option<String>>,
    /// Every tag delivery in arrival order; the last entry is the empty tag.
    tag_sequence: Vec<Option<String>>,
    /// Every description delivery in arrival order; the last entry is empty.
    description_sequence: Vec<Option<String>>,
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
        self.icon_sequence
            .push(icon.as_ref().and_then(ToplevelIcon::name));
        // `Some`-only: the reset (`None`) the driver sends after the set must
        // not clobber the set delivery the assertions below read.
        if let Some(icon) = icon {
            self.icon_has_buffer = Some(icon.buffer().is_some());
            // Take a second reference and drop the first; both must remain valid.
            let cloned = icon.clone();
            self.icon_clone_ok = Some(cloned.name() == icon.name());
        }
    }

    fn toplevel_tag_changed(&mut self, _toplevel: &Toplevel<'_>, tag: Option<&str>) {
        self.tag_sequence.push(tag.map(str::to_owned));
    }

    fn toplevel_description_changed(
        &mut self,
        _toplevel: &Toplevel<'_>,
        description: Option<&str>,
    ) {
        self.description_sequence
            .push(description.map(str::to_owned));
    }
}

/// A client sets an icon (name + buffer) and a tag/description on a live
/// toplevel, then resets the icon and clears both strings; the owned
/// [`ToplevelIcon`], both strings, the `None` reset and both empty values
/// reach the handler in order.
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
        icon_has_buffer: None,
        icon_clone_ok: None,
        icon_sequence: Vec::new(),
        tag_sequence: Vec::new(),
        description_sequence: Vec::new(),
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
        app.icon_sequence.first(),
        Some(&Some("wlr-test-icon".to_owned())),
        "the icon's stock name reached the handler first: {:?}",
        app.icon_sequence
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
        app.tag_sequence.first(),
        Some(&Some("wlr-test-tag".to_owned())),
        "the tag reached the handler first: {:?}",
        app.tag_sequence
    );
    assert_eq!(
        app.description_sequence.first(),
        Some(&Some("WlR test description".to_owned())),
        "the description reached the handler first: {:?}",
        app.description_sequence
    );
    // The reset the driver sends after the set: the `Some` delivery first,
    // then exactly one `None` reset after it.
    assert!(
        app.icon_sequence
            .first()
            .is_some_and(|name| name.as_deref() == Some("wlr-test-icon")),
        "the set delivery arrived first: {:?}",
        app.icon_sequence
    );
    assert_eq!(
        app.icon_sequence
            .iter()
            .filter(|name| name.is_none())
            .count(),
        1,
        "exactly one reset delivery: {:?}",
        app.icon_sequence
    );
    assert_eq!(
        app.icon_sequence.last(),
        Some(&None),
        "the reset arrives after the set: {:?}",
        app.icon_sequence
    );
    // The empty tag and description the driver sends after the non-empty
    // ones: empty strings are legal values and must be observed in order.
    assert_eq!(
        app.tag_sequence,
        vec![Some("wlr-test-tag".to_owned()), Some(String::new()),],
        "the set tag arrives first and the empty tag after it"
    );
    assert_eq!(
        app.description_sequence,
        vec![Some("WlR test description".to_owned()), Some(String::new()),],
        "the set description arrives first and the empty one after it"
    );
}

// ---------------------------------------------------------------------------
// Icon clear through the shared driver, without the tag manager
// ---------------------------------------------------------------------------

/// The shared icon set/reset flow, driven without creating a tag manager: the
/// handler still sees the `Some` delivery first and exactly one `None` reset
/// after it. The icon-without-tag coverage stays, while the driver itself
/// stays single-sourced in `common::client`.
#[test]
fn clearing_the_icon_delivers_none_after_some() {
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
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        client: Some(common::client::spawn_toplevel_meta_with(&socket, false)),
        icon_has_buffer: None,
        icon_clone_ok: None,
        icon_sequence: Vec::new(),
        tag_sequence: Vec::new(),
        description_sequence: Vec::new(),
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

    assert!(
        app.icon_sequence
            .first()
            .is_some_and(|name| name.as_deref() == Some("wlr-test-icon")),
        "the set delivery arrived first: {:?}",
        app.icon_sequence
    );
    assert_eq!(
        app.icon_sequence
            .iter()
            .filter(|name| name.is_none())
            .count(),
        1,
        "exactly one reset delivery: {:?}",
        app.icon_sequence
    );
    assert_eq!(
        app.icon_sequence.last(),
        Some(&None),
        "the reset arrives after the set: {:?}",
        app.icon_sequence
    );
    assert!(
        app.tag_sequence.is_empty() && app.description_sequence.is_empty(),
        "no tag manager, no tag or description deliveries: {:?} {:?}",
        app.tag_sequence,
        app.description_sequence
    );
}

// ---------------------------------------------------------------------------
// Icon forms: name-only, buffer-only, two buffers
// ---------------------------------------------------------------------------

/// What [`run_icon_form`] observed: the icon's stock name and its first
/// buffer's size. Each half is `None` when no icon arrived at all and
/// inner-`None` when that half was absent — the alias keeps the signature
/// under clippy's `type_complexity` lint.
type IconFormObserved = (Option<Option<String>>, Option<Option<(i32, i32)>>);

/// Records the single icon each form driver assigns: its stock name and its
/// first buffer's size, if any.
struct IconFormApp {
    client: Option<JoinHandle<common::client::ClientEvents>>,
    icon_name: Option<Option<String>>,
    icon_size: Option<Option<(i32, i32)>>,
}

impl wlr::OutputHandler for IconFormApp {}
impl wlr::FdHandler for IconFormApp {}
impl wlr::SeatHandler for IconFormApp {}

impl wlr::LoopHandler for IconFormApp {
    fn should_stop(&mut self) -> bool {
        self.client.as_ref().is_some_and(|h| h.is_finished())
    }
}

impl wlr::ToplevelHandler for IconFormApp {
    fn toplevel_icon_changed(&mut self, _toplevel: &Toplevel<'_>, icon: Option<ToplevelIcon>) {
        // Each form driver assigns exactly one icon and never resets it, so
        // the first `Some` delivery is the whole observation.
        if self.icon_name.is_none()
            && let Some(icon) = icon
        {
            self.icon_name = Some(icon.name());
            self.icon_size = Some(
                icon.buffer()
                    .map(|buffer| (buffer.width(), buffer.height())),
            );
        }
    }
}

/// Run one [`IconForm`] driver against a server with only the icon manager
/// and return the observed (name, first-buffer size).
fn run_icon_form(form: IconForm) -> IconFormObserved {
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
    runtime.create_xdg_shell(&display, 7).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = IconFormApp {
        client: Some(common::client::spawn_toplevel_icon_form(&socket, form)),
        icon_name: None,
        icon_size: None,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");
    app.client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");
    (app.icon_name, app.icon_size)
}

/// A name-only icon reaches the handler with its name and no buffer.
#[test]
fn icon_name_without_a_buffer_reads_back() {
    let (name, size) = run_icon_form(IconForm::NameOnly);
    assert_eq!(
        name,
        Some(Some("wlr-form-name".to_owned())),
        "the stock name reached the handler"
    );
    assert_eq!(size, Some(None), "a name-only icon carries no buffer");
}

/// A buffer-only icon reaches the handler with no name and its pixel buffer.
#[test]
fn icon_buffer_without_a_name_reads_back() {
    let (name, size) = run_icon_form(IconForm::BufferOnly);
    assert_eq!(name, Some(None), "a buffer-only icon has no name");
    assert_eq!(
        size,
        Some(Some((64, 64))),
        "the pixel buffer reached the handler at its size"
    );
}

/// With two buffers the handler reads the first the client added.
#[test]
fn icon_first_buffer_wins() {
    let (name, size) = run_icon_form(IconForm::TwoBuffers);
    assert_eq!(name, Some(None), "no name was set");
    assert_eq!(
        size,
        Some(Some((64, 64))),
        "buffer() returns the first-added buffer, not the 32x32 second"
    );
}
