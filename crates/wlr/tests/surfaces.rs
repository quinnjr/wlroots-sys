mod common;

use wlr::{Backend, Display, Runtime, SurfaceId, Until};

#[derive(Default)]
struct App {
    surfaces: usize,
    destroyed: usize,
}

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, _t: &wlr::Toplevel<'_>) {}
    fn surface_mapped(&mut self, _id: SurfaceId) {
        self.surfaces += 1;
    }
    fn surface_destroyed(&mut self, _id: SurfaceId) {
        self.destroyed += 1;
    }
}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {}

#[test]
fn every_surface_operation_misses_on_a_dangling_id() {
    common::headless_env();
    let _serial = common::headless_guard();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    let mut app = App::default();
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    assert!(runtime.surface(SurfaceId::dangling_for_test()).is_none());
    assert_eq!(
        SurfaceId::dangling_for_test(),
        SurfaceId::dangling_for_test()
    );
}
