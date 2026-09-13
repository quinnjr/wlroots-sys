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
    common::headless_env();
    let _serial = common::headless_guard();
    common::isolated_runtime_dir(); // XDG_RUNTIME_DIR must exist before the socket is bound
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg-shell");
    let socket = display.add_socket_auto().expect("socket");

    let handle = common::client::spawn(&socket, |state, qh| state.create_toplevel(qh));

    let mut app = App::default();
    for _ in 0..40 {
        backend
            .run_all(&display, &mut app, &runtime, Until::Turns(50))
            .expect("run_all");
        if app.toplevels > 0 {
            break;
        }
    }
    handle.join().expect("client thread");

    assert!(
        app.toplevels >= 1,
        "server should observe the client's toplevel"
    );
}
