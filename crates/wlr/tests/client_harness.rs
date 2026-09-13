mod common;

use wlr::{Backend, Display, Runtime, Until};

#[derive(Default)]
struct App {
    toplevels: usize,
}

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, _t: &wlr::Toplevel<'_>) {
        self.toplevels += 1;
    }
}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {}

#[test]
fn a_real_client_creates_a_toplevel_the_server_observes() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir(); // XDG_RUNTIME_DIR must exist before the socket is bound
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let handle = common::client::spawn(&socket, |state, qh| state.create_toplevel(qh));

    let mut app = App::default();
    // Drive the server until the client thread finishes rather than for a fixed
    // count. `Until::Turns` is non-blocking, so under load the client may not
    // have been scheduled before any fixed budget of polls runs out; the server
    // would then block in `join` while the client blocks forever in
    // `roundtrip`. `is_finished` couples the two: the loop cannot end before the
    // client is done, and a panicking client marks the handle finished, turning
    // a hang into a clean `join` error.
    while !handle.is_finished() {
        backend
            .run_all(&display, &mut app, &runtime, Until::Turns(50))
            .expect("run_all");
    }
    let events = handle.join().expect("client thread");

    assert_eq!(
        app.toplevels, 1,
        "server should observe exactly one toplevel from the client"
    );
    assert!(
        events.configure_events >= 1,
        "the client should have received an xdg_surface configure"
    );
    assert!(
        events.acked_configures >= 1,
        "the client should have acked the configure it received"
    );
}
