//! What an fd source asks to be woken for, and what it was woken for.
//!
//! libwayland spells both as a bitmask of `WL_EVENT_*`. Those constants live
//! in `wayland-server-core.h` as a plain C enum, which neither `wayland-sys`
//! nor this branch's bindgen allowlist binds, so they are restated here. They
//! are ABI, fixed since libwayland 1.0.
//!
//! Three of the four bits are pinned against libwayland's own behaviour,
//! rather than merely against this comment, by tests in this module's
//! `mod tests` that drive a real headless backend over a real pipe:
//! `WRITABLE` (a pipe's write end, registered with [`Interest::WRITABLE`],
//! comes back writable), `HANGUP` (a read end whose peer is already gone
//! comes back hung up), and that [`Interest::READ_WRITE`] on a write-only fd
//! is answered `writable` but not `readable`. `READABLE` is pinned instead by
//! `tests/fd_sources.rs`, one level up, since it exercises
//! `Backend::run_all`'s full delivery path rather than just this module.
//! `ERROR` has no such test: nothing here can provoke a genuine fd error
//! deterministically without a scenario invasive enough to undermine the
//! rest of the suite, so its value is untested beyond the decode logic
//! `readiness_decodes_each_bit_independently` below already covers.

/// `WL_EVENT_READABLE`.
pub(crate) const READABLE: u32 = 0x01;
/// `WL_EVENT_WRITABLE`.
pub(crate) const WRITABLE: u32 = 0x02;
/// `WL_EVENT_HANGUP`.
pub(crate) const HANGUP: u32 = 0x04;
/// `WL_EVENT_ERROR`.
pub(crate) const ERROR: u32 = 0x08;

/// What an fd source asks the event loop to watch for.
///
/// Deliberately not a `bitflags` type and deliberately without public fields:
/// the set of things libwayland lets a source *request* is exactly readable
/// and writable (hangup and error are always reported and cannot be asked
/// for), so an open-ended flags type would advertise combinations that do not
/// exist. The three constants are the whole domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interest {
    mask: u32,
}

impl Interest {
    /// Wake when the fd has data to read.
    pub const READABLE: Interest = Interest { mask: READABLE };
    /// Wake when the fd can accept a write.
    pub const WRITABLE: Interest = Interest { mask: WRITABLE };
    /// Wake for either.
    pub const READ_WRITE: Interest = Interest {
        mask: READABLE | WRITABLE,
    };

    pub(crate) fn mask(self) -> u32 {
        self.mask
    }
}

/// Why an fd source was woken.
///
/// Hangup and error arrive whether or not they were asked for, so this is a
/// superset of the [`Interest`] that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Readiness {
    mask: u32,
}

impl Readiness {
    pub(crate) fn from_mask(mask: u32) -> Readiness {
        Readiness { mask }
    }

    /// The fd has data to read.
    pub fn readable(self) -> bool {
        self.mask & READABLE != 0
    }

    /// The fd can accept a write.
    pub fn writable(self) -> bool {
        self.mask & WRITABLE != 0
    }

    /// The peer closed its end.
    pub fn hangup(self) -> bool {
        self.mask & HANGUP != 0
    }

    /// The fd is in an error state.
    pub fn error(self) -> bool {
        self.mask & ERROR != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_decodes_each_bit_independently() {
        let r = Readiness::from_mask(READABLE | HANGUP);
        assert!(r.readable());
        assert!(r.hangup());
        assert!(!r.writable());
        assert!(!r.error());
    }

    #[test]
    fn read_write_is_the_union_of_the_two_singletons() {
        assert_eq!(
            Interest::READ_WRITE.mask(),
            Interest::READABLE.mask() | Interest::WRITABLE.mask()
        );
    }

    /// A minimal `Handlers` impl that records whatever `Readiness` its one
    /// registered source was woken with, then asks `run_all` to stop.
    #[derive(Default)]
    struct Recorder {
        seen: Option<Readiness>,
    }

    impl crate::OutputHandler for Recorder {}
    impl crate::ToplevelHandler for Recorder {}
    impl crate::SeatHandler for Recorder {}

    impl crate::FdHandler for Recorder {
        fn fd_ready(
            &mut self,
            _source: crate::SourceId,
            _fd: std::os::fd::BorrowedFd<'_>,
            readiness: Readiness,
        ) {
            self.seen = Some(readiness);
        }
    }

    impl crate::LoopHandler for Recorder {
        fn should_stop(&mut self) -> bool {
            self.seen.is_some()
        }
    }

    /// Serialises this module's real-backend tests against each other.
    ///
    /// `Backend::autocreate` reads `WLR_BACKENDS` via `getenv`, and libtest
    /// runs `#[test]` functions on parallel threads by default, so an
    /// unguarded `setenv` racing another thread's `getenv` is undefined
    /// behaviour. `headless_env` is the single call site for both, and every
    /// test below calls it before touching a `Display` or a `Backend`: the
    /// `Once` serialises the one write against every read *this module*
    /// makes, and no test anywhere else in this crate's `#[cfg(test)]` code
    /// touches `WLR_BACKENDS` or calls `autocreate` (`tests/fd_sources.rs`,
    /// `tests/headless.rs` and `tests/reentrancy.rs` do, but each is compiled
    /// to its own binary — a separate process, with its own environment — so
    /// none of them can race a write made here). That is what makes "no other
    /// reader exists" true rather than merely convenient.
    fn headless_env() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // SAFETY: `Once::call_once` runs this closure at most once and
            // blocks every other caller of `call_once` on this `Once` until
            // it returns, so no concurrent `getenv` from another call to
            // `headless_env` in this binary can observe a torn write. This
            // function's own doc is the argument for why no reader outside
            // `call_once`'s reach can race it either.
            unsafe {
                std::env::set_var("WLR_BACKENDS", "headless");
                std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            }
        });
    }

    /// Pins [`Interest::WRITABLE`] and [`Readiness::writable`] against a real
    /// event loop: a freshly created pipe's write end has an empty buffer, so
    /// libwayland must report it writable on the very first turn.
    #[test]
    fn a_pipes_write_end_registered_for_writable_comes_back_writable() {
        headless_env();
        let display = crate::Display::new().expect("display");
        let backend = crate::Backend::autocreate(&display.event_loop()).expect("backend");
        let runtime = crate::Runtime::new().expect("runtime");

        let (_read, write) = rustix::pipe::pipe().expect("pipe");
        runtime.add_fd(write, Interest::WRITABLE);

        let mut app = Recorder::default();
        backend
            .run_all(&display, &mut app, &runtime, crate::Until::Stop)
            .expect("run_all");

        let readiness = app.seen.expect("the write end must have woken");
        assert!(readiness.writable(), "a fresh pipe's write end must be writable");
    }

    /// Pins [`Readiness::hangup`] against a real event loop. The peer (the
    /// write end) is dropped before the source is even registered, so the
    /// very first turn must report the read end hung up.
    #[test]
    fn a_read_end_whose_peer_is_already_gone_comes_back_hung_up() {
        headless_env();
        let display = crate::Display::new().expect("display");
        let backend = crate::Backend::autocreate(&display.event_loop()).expect("backend");
        let runtime = crate::Runtime::new().expect("runtime");

        let (read, write) = rustix::pipe::pipe().expect("pipe");
        drop(write);
        runtime.add_fd(read, Interest::READABLE);

        let mut app = Recorder::default();
        backend
            .run_all(&display, &mut app, &runtime, crate::Until::Stop)
            .expect("run_all");

        let readiness = app.seen.expect("the read end must have woken");
        assert!(
            readiness.hangup(),
            "a read end with no writer left must be reported as hung up"
        );
    }

    /// Pins [`Interest::READ_WRITE`] against a real event loop: asking for
    /// both bits on a write-only fd must still deliver only the one that
    /// applies, which is what tells this apart from a registration that
    /// silently ignored the `WRITABLE` half.
    #[test]
    fn read_write_interest_on_a_write_only_fd_comes_back_writable_not_readable() {
        headless_env();
        let display = crate::Display::new().expect("display");
        let backend = crate::Backend::autocreate(&display.event_loop()).expect("backend");
        let runtime = crate::Runtime::new().expect("runtime");

        let (_read, write) = rustix::pipe::pipe().expect("pipe");
        runtime.add_fd(write, Interest::READ_WRITE);

        let mut app = Recorder::default();
        backend
            .run_all(&display, &mut app, &runtime, crate::Until::Stop)
            .expect("run_all");

        let readiness = app.seen.expect("the write end must have woken");
        assert!(
            readiness.writable(),
            "asking for both interests must still deliver the applicable bit"
        );
        assert!(
            !readiness.readable(),
            "a write-only fd is never readable, even when Interest asked for it"
        );
    }
}
