use std::os::fd::AsFd;

use wlr::{Display, SyncFlags, SyncTimeline};

/// A waiter's `Drop` calls `wl_event_source_remove` on a source registered in
/// the display's event loop, so a waiter that outlived the display would free
/// through a destroyed loop. `SyncWaiter<'l>` borrows the loop — and therefore
/// the display behind it — which makes that this compile error.
fn main() {
    let node = std::fs::File::open("/dev/null").expect("a descriptor to name");
    let timeline = SyncTimeline::create(node.as_fd()).expect("timeline");

    let display = Display::new().expect("display");
    let waiter = display
        .event_loop()
        .wait_for_timeline(&timeline, 1, SyncFlags::NONE, || {})
        .expect("wait");

    drop(display);

    let _ = waiter;
}
