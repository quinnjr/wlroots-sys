//! A pipe registered as an fd source is delivered to `FdHandler::fd_ready`
//! inside `Backend::run_all`, and `LoopHandler::should_stop` ends the run.
//!
//! Uses the headless backend so this needs no GPU, no seat, and no display.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

/// Handlers must not panic — they run under an `extern "C"` frame, so an
/// assertion that fires aborts the process instead of failing this test.
/// Everything worth asserting is recorded here and checked afterwards.
#[derive(Default)]
struct App {
    woke: Vec<wlr::SourceId>,
    bytes: Vec<u8>,
    readable: u32,
    stop: bool,
}

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::SessionLockHandler for App {}

impl wlr::FdHandler for App {
    fn fd_ready(&mut self, source: wlr::SourceId, fd: BorrowedFd<'_>, readiness: wlr::Readiness) {
        self.woke.push(source);
        if readiness.readable() {
            self.readable += 1;
        }
        let mut buf = [0u8; 8];
        // `read` on a raw fd through a borrowed `File` would close it on
        // drop; `rustix::io::read` borrows and never closes.
        if let Ok(n) = rustix::io::read(fd, &mut buf) {
            self.bytes.extend_from_slice(&buf[..n]);
        }
        self.stop = true;
    }
}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.stop
    }
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let (read, write) = rustix::pipe::pipe().expect("pipe");
    (read, write)
}

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`.
///
/// `Backend::autocreate` reads `WLR_BACKENDS` via `getenv`, and libtest runs
/// `#[test]` functions on parallel threads by default, so an unguarded
/// `setenv` racing another thread's `getenv` is undefined behaviour — and,
/// separately, whichever test's `setenv` loses that race would get whatever
/// backend wlroots picks with no `WLR_BACKENDS` set at all, which on a
/// machine with no GPU is not "headless" and not deterministic.
///
/// This is the single call site for both operations. **Every test in this
/// file must call this before constructing a `Display` or a `Backend`** —
/// there is no other guard; a test that calls `autocreate` directly reopens
/// exactly the hazard this function exists to close. The `Once` serialises
/// the one write against every read this binary makes, since `autocreate` is
/// reached from nowhere else in this file — and this integration test is
/// compiled to its own binary, its own process with its own environment, so
/// nothing outside this file can race the write either. (`wlr`'s own library
/// unit tests carry a sibling copy of this same helper, in
/// `src/interest.rs`'s `mod tests`, for the identical reason — that binary is
/// a different process too, and cannot see this one's environment.)
fn headless_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once::call_once` runs this closure at most once and blocks
        // every other caller of `call_once` on this `Once` until it returns,
        // so no concurrent `getenv` from another test's call to
        // `headless_env` can observe a torn write. This function's own doc
        // comment is the argument for why no other reader exists to race.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
        }
    });
}

#[test]
fn a_registered_fd_wakes_its_handler_and_should_stop_ends_the_run() {
    headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");

    let (read, mut write_end) = {
        let (r, w) = pipe();
        (r, std::fs::File::from(w))
    };
    let read_fd = read.as_raw_fd();
    let source = runtime.add_fd(read, wlr::Interest::READABLE);

    write_end.write_all(b"q").expect("write");

    let mut app = App::default();
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("run_all");

    assert_eq!(app.woke, vec![source], "exactly the registered source woke");
    assert_eq!(
        app.readable, 1,
        "the readiness passed to the handler says readable"
    );
    assert_eq!(app.bytes, b"q", "the handler got the very fd it registered");
    let _ = read_fd;
}

#[test]
fn turns_bounds_a_run_that_nothing_ever_stops() {
    headless_env();

    // No source, no stop: `Until::Turns` must return rather than block.
    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    let mut app = App::default();

    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(3))
        .expect("run_all");

    assert!(
        app.woke.is_empty(),
        "nothing was registered, so nothing woke"
    );
}

#[test]
fn a_source_registered_after_a_run_is_armed_by_the_next_run() {
    headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    let mut app = App::default();

    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(1))
        .expect("first run");

    let (read, w) = pipe();
    let mut write_end = std::fs::File::from(w);
    let source = runtime.add_fd(read, wlr::Interest::READABLE);
    write_end.write_all(b"z").expect("write");

    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("second run");

    assert_eq!(
        app.woke,
        vec![source],
        "sources live on the Runtime, so a later run re-arms them"
    );
}

/// `read` above needs the fd without taking ownership of it; keep the
/// dependency honest by proving the import is used.
#[test]
fn reading_a_borrowed_fd_does_not_close_it() {
    let (read, w) = pipe();
    let mut write_end = std::fs::File::from(w);
    write_end.write_all(b"ab").expect("write");
    let mut buf = [0u8; 2];
    let n = rustix::io::read(&read, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"ab");
    let mut again = std::fs::File::from(read);
    let mut rest = String::new();
    drop(write_end);
    again.read_to_string(&mut rest).expect("read again");
    assert!(rest.is_empty(), "the fd survived the borrowed read");
}

/// `run_all` must refuse a `Display` that does not own the backend's loop —
/// dispatching a different display's loop, and flushing a different
/// display's clients, would otherwise produce silence rather than an error.
#[test]
fn run_all_refuses_a_display_that_does_not_own_this_backends_loop() {
    headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");

    let other_display = wlr::Display::new().expect("other display");
    let mut app = App::default();

    let err = backend
        .run_all(&other_display, &mut app, &runtime, wlr::Until::Turns(1))
        .expect_err("a display that does not own this backend's loop must be refused");

    assert_eq!(err, wlr::Error::Mismatch("Backend::run_all"));
}
