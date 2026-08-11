//! A display can be created, dispatched and torn down.

#[test]
fn display_creates_and_dispatches() {
    let display = wlr::Display::new().expect("wl_display_create failed");
    let loop_ = display.event_loop();

    // A zero timeout returns immediately whether or not anything was ready; we
    // only care that dispatching does not fault.
    loop_.dispatch(0).expect("dispatch failed");
}

/// Repeated create/drop must not fault or panic.
///
/// This does **not** prove leak-freedom, despite what an earlier name for this
/// test claimed: there is no assertion here that could fail if `Drop for
/// Display` were emptied out, or if `wl_display_destroy` were replaced with
/// `std::mem::forget`, and nothing external backs it either — CI's Miri job is
/// scoped to `dispatch::tests` (Miri cannot run this at all; it calls into
/// libwayland), and there is no valgrind/ASan/LSan step. What this does prove:
/// `wl_display_create`/`wl_display_destroy` can be called in a tight loop
/// without wlroots or libwayland aborting the process.
#[test]
fn a_display_can_be_created_and_dropped_repeatedly() {
    for _ in 0..8 {
        let display = wlr::Display::new().expect("wl_display_create failed");
        drop(display);
    }
}
