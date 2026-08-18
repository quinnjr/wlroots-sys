//! Register a pipe as an event source and stop the loop when it fires.
//!
//! The shape a compositor uses for shutdown: something outside the event loop
//! (a signal handler, a worker thread) writes a byte, the loop wakes, the
//! handler records it, and `should_stop` ends the run on the next turn.
//!
//! ```sh
//! WLR_BACKENDS=headless cargo run -p wlr --example fd_source
//! ```

use std::io::Write;
use std::os::fd::BorrowedFd;

#[derive(Default)]
struct App {
    stop: bool,
}

impl wlr::OutputHandler for App {}
impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::SessionLockHandler for App {}

impl wlr::FdHandler for App {
    fn fd_ready(&mut self, source: wlr::SourceId, fd: BorrowedFd<'_>, readiness: wlr::Readiness) {
        let mut buf = [0u8; 16];
        // Level-triggered: a handler that reads nothing is called again next
        // turn, forever.
        let n = rustix::io::read(fd, &mut buf).unwrap_or(0);
        println!(
            "source {source:?} woke: readable={} bytes={n}",
            readiness.readable()
        );
        self.stop = true;
    }
}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.stop
    }
}

fn main() -> wlr::Result<()> {
    let display = wlr::Display::new()?;
    let backend = wlr::Backend::autocreate(&display.event_loop())?;
    let runtime = wlr::Runtime::new()?;

    let (read, write) = rustix::pipe::pipe().expect("pipe");
    let id = runtime.add_fd(read, wlr::Interest::READABLE);
    println!("registered source {id:?}");

    let mut write_end = std::fs::File::from(write);
    write_end.write_all(b"quit").expect("write");

    let mut app = App::default();
    backend.run_all(&display, &mut app, &runtime, wlr::Until::Stop)?;
    println!("loop ended cleanly");
    Ok(())
}
