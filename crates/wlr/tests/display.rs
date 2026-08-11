//! A display can be created, dispatched and torn down.

#[test]
fn display_creates_and_dispatches() {
    let display = wlr::Display::new().expect("wl_display_create failed");
    let loop_ = display.event_loop();

    // A zero timeout returns immediately whether or not anything was ready; we
    // only care that dispatching does not fault.
    loop_.dispatch(0).expect("dispatch failed");
}

#[test]
fn display_is_dropped_without_leaking() {
    for _ in 0..8 {
        let display = wlr::Display::new().expect("wl_display_create failed");
        drop(display);
    }
}
