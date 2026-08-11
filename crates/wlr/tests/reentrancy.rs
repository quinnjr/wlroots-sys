//! A handler must not be able to drive the event loop while it holds a handle.
//!
//! This is the hole that borrow-scoping alone does not close, and it is worth
//! being precise about why. `Output<'h>`'s lifetime stops a handle *escaping*
//! the handler call. It says nothing about what may happen *during* the call —
//! and `EventLoop::dispatch` is public, safe, takes `&self`, and needs only a
//! `&Display`, which a handler's own state may perfectly well hold. So a
//! handler could re-enter wlroots' dispatching, let wlroots destroy and free the
//! output it was just handed, and go on using the handle. Use-after-free,
//! reachable from entirely safe code, with no `unsafe` written anywhere.
//!
//! `Dispatcher` therefore raises a thread-scoped flag for the duration of every
//! delivery, and `EventLoop::dispatch` refuses while it is set. This test drives
//! that through the public API only — a real display, a real headless backend, a
//! real announcement — because the guarantee is about what a consumer can reach,
//! and a unit test poking the flag directly would prove nothing about that.

/// Holds a `&Display`, which is the whole point: nothing in the signature of
/// `new_output` hands a handler the event loop, but a handler's own state can
/// carry one, and that is enough.
struct App<'d> {
    display: &'d wlr::Display,

    /// What `EventLoop::dispatch` returned when called from inside the handler.
    /// Recorded rather than asserted, because a handler runs under an
    /// `extern "C"` frame where a failing assertion aborts the process instead
    /// of failing the test.
    dispatch_from_handler: Option<wlr::Result<()>>,

    /// The name read from the handle *after* the reentrant call. Reading it is
    /// the use-after-free the refusal prevents; it is only sound here because
    /// the refusal means no wlroots code ran in between.
    name_after: Option<String>,
}

impl wlr::OutputHandler for App<'_> {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        // The exact sequence from the review: drive the loop from inside a
        // handler, then keep using the handle.
        self.dispatch_from_handler = Some(self.display.event_loop().dispatch(0));
        self.name_after = output.name();
    }
}

#[test]
fn a_handler_cannot_dispatch_the_event_loop() {
    // SAFETY: `set_var` is unsound only against a concurrent reader of the
    // environment on another thread. This is the only test in this binary, so
    // the harness has started no other test thread, and nothing here has yet
    // called into wlroots.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }

    let display = wlr::Display::new().expect("display");
    // Declared after `display` so it drops first; see `headless.rs`.
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let mut app = App {
        display: &display,
        dispatch_from_handler: None,
        name_after: None,
    };

    backend.run(&mut app, 4).expect("run");

    assert_eq!(
        app.dispatch_from_handler,
        Some(Err(wlr::Error::Reentrant("EventLoop::dispatch"))),
        "a handler that drives the event loop must be refused, and told which \
         entry point it re-entered — without the refusal, wlroots is free to \
         free the output the handler is still holding a handle to"
    );
    assert!(
        app.name_after.is_some(),
        "and the handle must still be usable afterwards, which is the property \
         the refusal exists to preserve"
    );

    // The refusal is scoped to the handler, not sticky. Asserted here rather
    // than in a test of its own, because this binary sets environment variables
    // and so must stay single-test; and it is not optional — a guard that set
    // the flag and never cleared it would satisfy every assertion above while
    // wedging the event loop shut for the rest of the process.
    assert_eq!(
        display.event_loop().dispatch(0),
        Ok(()),
        "with no handler on the stack there is nothing to refuse, or one \
         dispatched event would lock the loop out forever"
    );
}
