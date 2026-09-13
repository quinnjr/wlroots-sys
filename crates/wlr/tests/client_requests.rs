//! `Edges`, and `Runtime::configure_toplevel`'s by-id contract.
//!
//! A client-driven test — spawning a real toplevel and asserting the
//! `request_maximize`/`request_fullscreen`/`request_move`/`request_resize`
//! signals actually fire — needs a client library this workspace does not
//! depend on, same as `tests/toplevels.rs`'s own note. What is provable here
//! without one: the by-id mutator rejects an id no live toplevel has rather
//! than dereferencing it, `Edges` behaves as documented, and the new
//! `ToplevelHandler` methods are additive — an old, empty impl still
//! satisfies `Handlers`.

mod common;

#[test]
fn configure_toplevel_on_a_dead_id_is_none() {
    common::headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    assert_eq!(
        runtime.configure_toplevel(wlr::ToplevelId::dangling_for_test()),
        None
    );
}

#[test]
fn edges_default_is_empty_and_flags_read_back() {
    let e = wlr::Edges::default();
    assert!(e.is_empty());
    let e = wlr::Edges {
        left: true,
        ..Default::default()
    };
    assert!(!e.is_empty());
}

#[test]
fn a_defaulted_handler_still_satisfies_handlers() {
    // Compile-time proof the new methods are additive: an empty impl block
    // from the 0.20.4 era still compiles and still satisfies Handlers.
    struct Old;
    impl wlr::OutputHandler for Old {}
    impl wlr::ToplevelHandler for Old {}
    impl wlr::SeatHandler for Old {}
    impl wlr::FdHandler for Old {}
    impl wlr::LoopHandler for Old {}
    fn takes_handlers<S: wlr::Handlers>(_s: &S) {}
    takes_handlers(&Old);
}
