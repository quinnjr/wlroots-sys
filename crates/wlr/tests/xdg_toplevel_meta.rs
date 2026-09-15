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

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::xdg::toplevel_icon::v1::client::{
    xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1,
};
use wlr::{Backend, Display, Runtime, Toplevel, ToplevelIcon, Until};

/// Both manager globals create once and refuse a second create, and the
/// size-preference call is safe before and after the manager exists.
#[test]
fn icon_and_tag_managers_create_once() {
    let _serial = common::headless_guard();
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
    /// The first `Some` icon's `name()`: recorded only off `Some` deliveries
    /// so the reset the driver sends afterwards cannot clobber it — the
    /// `client_drives_icon_and_tag_events` assertions read the set delivery
    /// through this, the reset through [`App::icon_sequence`].
    icon_name: Option<Option<String>>,
    /// Whether the first delivered icon carried a pixel buffer; `Some`-only
    /// like [`App::icon_name`].
    icon_has_buffer: Option<bool>,
    /// Whether a clone of the delivered icon was independent (the refcount
    /// path a compositor keeping two references would use).
    icon_clone_ok: Option<bool>,
    /// The first tag delivery; `Some`-sequence-first so the empty tag the
    /// driver sends afterwards cannot clobber the set delivery the original
    /// assertions read.
    tag: Option<Option<String>>,
    /// The first description delivery; first-seen like [`App::tag`].
    description: Option<Option<String>>,
    /// Every icon delivery in arrival order (`None` = the reset): the clear
    /// path is asserted off this sequence.
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
            self.icon_name = Some(icon.name());
            self.icon_has_buffer = Some(icon.buffer().is_some());
            // Take a second reference and drop the first; both must remain valid.
            let cloned = icon.clone();
            self.icon_clone_ok = Some(cloned.name() == icon.name());
        }
    }

    fn toplevel_tag_changed(&mut self, _toplevel: &Toplevel<'_>, tag: Option<&str>) {
        self.tag_sequence.push(tag.map(str::to_owned));
        // First-seen: the empty tag the driver sends after the set must not
        // clobber the set delivery the original assertions read; the empty
        // value is asserted off `tag_sequence`.
        if self.tag.is_none() {
            self.tag = Some(tag.map(str::to_owned));
        }
    }

    fn toplevel_description_changed(
        &mut self,
        _toplevel: &Toplevel<'_>,
        description: Option<&str>,
    ) {
        self.description_sequence
            .push(description.map(str::to_owned));
        // First-seen, as for the tag.
        if self.description.is_none() {
            self.description = Some(description.map(str::to_owned));
        }
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
        icon_name: None,
        icon_has_buffer: None,
        icon_clone_ok: None,
        tag: None,
        description: None,
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
// Icon clear: set, then reset to the default icon
// ---------------------------------------------------------------------------

/// Test-local client state for the icon-clear leg: the shared
/// `spawn_toplevel_meta` only sets the icon, so this file owns the variant
/// that resets it afterwards.
struct IconState;

macro_rules! icon_empty_dispatch {
    ($($t:ty),+) => {$(
        impl Dispatch<$t, ()> for IconState {
            fn event(
                _state: &mut Self,
                _proxy: &$t,
                _event: <$t as wayland_client::Proxy>::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    )+};
}

icon_empty_dispatch!(
    wl_compositor::WlCompositor,
    wl_surface::WlSurface,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    xdg_toplevel::XdgToplevel,
    xdg_toplevel_icon_v1::XdgToplevelIconV1,
    xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1
);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for IconState {
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

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for IconState {
    fn event(
        _state: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for IconState {
    fn event(
        _state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
        }
    }
}

/// Like the shared icon/tag driver, but resets the icon after setting it: a
/// named icon with a pixel buffer, applied by commit, then `set_icon` with a
/// null icon and a second commit. The server must deliver the `Some` icon
/// first and exactly one `None` reset after it.
fn spawn_icon_then_clear(socket: &str) -> std::thread::JoinHandle<common::client::ClientEvents> {
    use std::os::fd::AsFd as _;
    let path = common::isolated_runtime_dir().join(socket);
    let shm_path = common::isolated_runtime_dir()
        .join(format!("wlr-rs-shm-{}-icon-clear", std::process::id()));
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
            registry_queue_init::<IconState>(&conn).expect("registry queue init");
        let qh = queue.handle();
        let mut state = IconState;

        let compositor: wl_compositor::WlCompositor =
            globals.bind(&qh, 1..=6, ()).expect("bind wl_compositor");
        let wm_base: xdg_wm_base::XdgWmBase =
            globals.bind(&qh, 1..=6, ()).expect("bind xdg_wm_base");
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
        let icon_manager: xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind xdg_toplevel_icon_manager_v1");
        let events = common::client::ClientEvents::default();

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server announces the toplevel");

        const W: i32 = 64;
        const H: i32 = 64;
        const STRIDE: i32 = W * 4;
        let size = STRIDE * H;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&shm_path)
            .expect("create shm backing file");
        file.set_len(size as u64).expect("size shm backing file");
        let pool = shm.create_pool(file.as_fd(), size, &qh, ());
        let buffer = pool.create_buffer(0, W, H, STRIDE, wl_shm::Format::Argb8888, &qh, ());
        let icon = icon_manager.create_icon(&qh, ());
        icon.set_name("wlr-test-icon".to_owned());
        icon.add_buffer(&buffer, 1);
        icon_manager.set_icon(&toplevel, Some(&icon));
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server applies the icon");

        // The reset: a null icon, applied by the commit that follows, which
        // wlroots forwards as `set_icon` with a null icon.
        icon_manager.set_icon(&toplevel, None);
        surface.commit();
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server applies the reset");
        queue
            .roundtrip(&mut state)
            .expect("roundtrip so the server drains");

        drop((
            icon,
            icon_manager,
            buffer,
            pool,
            file,
            surface,
            xdg_surface,
            toplevel,
            shm,
            wm_base,
            compositor,
        ));
        events
    })
}

/// A client sets an icon and then resets it; the handler sees the `Some`
/// delivery first and exactly one `None` reset after it.
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
        client: Some(spawn_icon_then_clear(&socket)),
        icon_name: None,
        icon_has_buffer: None,
        icon_clone_ok: None,
        tag: None,
        description: None,
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
}
