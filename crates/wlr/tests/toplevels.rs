//! The xdg-shell wiring, against a real headless compositor with no client.
//!
//! A client-driven test — spawning a real toplevel against this compositor
//! and asserting the configure it receives — is a parity-milestone goal, not
//! a slice one: it needs a client library this workspace does not depend on.
//! What is provable here is everything up to the client: the shell global
//! exists, the id-keyed mutators reject ids that were never issued rather
//! than dereferencing them, and the handler set is implementable.

mod common;

#[derive(Default)]
struct App {
    new_toplevels: Vec<wlr::ToplevelId>,
    mapped: Vec<wlr::ToplevelId>,
    unmapped: Vec<wlr::ToplevelId>,
    destroyed: Vec<wlr::ToplevelId>,
    turns: u32,
}

impl wlr::OutputHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.turns += 1;
        self.turns >= 4
    }
}

impl wlr::ToplevelHandler for App {
    fn new_toplevel(&mut self, toplevel: &wlr::Toplevel<'_>) {
        self.new_toplevels.push(toplevel.id());
    }
    fn mapped(&mut self, toplevel: &wlr::Toplevel<'_>) {
        self.mapped.push(toplevel.id());
    }
    fn unmapped(&mut self, id: wlr::ToplevelId) {
        self.unmapped.push(id);
    }
    fn toplevel_destroyed(&mut self, id: wlr::ToplevelId) {
        self.destroyed.push(id);
    }
}

#[test]
fn an_xdg_shell_can_be_created_and_a_run_survives_it() {
    let _serial = common::headless_guard();
    common::headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("xdg shell");

    // `Until::Turns`, not `Until::Stop`: nothing in this test ever calls
    // `Output::schedule_frame`, so with no client and no rendering damage
    // this compositor produces no further wakeup once its one announced
    // output has settled — `Until::Stop`'s blocking `wl_event_loop_dispatch`
    // would then wait forever. `Until::Turns` dispatches non-blocking turns
    // instead (see `fd_sources.rs`'s identical comment on the same
    // no-activity case), so this returns after four either way: `should_stop`
    // above would also end it, but `Turns` is what makes that unnecessary
    // for termination rather than merely a redundant belt-and-braces check.
    let mut app = App::default();
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(4))
        .expect("run_all");

    // No client connected, so nothing was announced — the point is that the
    // shell's listener was registered and torn down without faulting.
    assert!(app.new_toplevels.is_empty());
    assert!(app.mapped.is_empty());
    assert!(app.unmapped.is_empty());
    assert!(app.destroyed.is_empty());
}

#[test]
fn creating_the_shell_twice_is_refused_rather_than_leaking_a_second_global() {
    let _serial = common::headless_guard();
    common::headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    // `create_xdg_shell` now refuses before `init_graphics` (see its own
    // doc), so this is required to reach the "twice" case at all.
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("first");
    assert!(
        matches!(
            runtime.create_xdg_shell(&display, 6),
            Err(wlr::Error::Operation(_))
        ),
        "a second xdg_wm_base global would make the compositor advertise two"
    );
}

#[test]
fn creating_the_shell_before_init_graphics_is_refused_rather_than_hanging_every_client() {
    let _serial = common::headless_guard();
    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    assert!(
        matches!(
            runtime.create_xdg_shell(&display, 6),
            Err(wlr::Error::Operation(_))
        ),
        "a shell with nowhere to put a toplevel in the scene must be refused \
         at setup time, not leave every client hanging on an unanswered \
         initial commit"
    );
}

#[test]
fn every_by_id_mutator_reports_an_unknown_id_rather_than_dereferencing_it() {
    let runtime = wlr::Runtime::new().expect("runtime");
    let ghost = wlr::ToplevelId::dangling_for_test();

    assert_eq!(runtime.set_toplevel_size(ghost, 100, 100), None);
    assert_eq!(runtime.set_toplevel_activated(ghost, true), None);
    assert_eq!(runtime.set_toplevel_maximized(ghost, true), None);
    assert_eq!(runtime.set_toplevel_fullscreen(ghost, true), None);
    assert_eq!(runtime.set_toplevel_position(ghost, 1, 2), None);
    assert_eq!(runtime.set_toplevel_visible(ghost, false), None);
    assert_eq!(runtime.raise_toplevel(ghost), None);
    assert_eq!(runtime.close_toplevel(ghost), None);
}
