use std::sync::Once;

fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    });
}

#[test]
fn in_toplevel_rect_on_a_dead_id_is_none() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let dead = wlr::ToplevelId::dangling_for_test();
    assert_eq!(
        runtime.add_rect_in_toplevel(dead, 10, 10, [1.0, 0.0, 0.0, 1.0]),
        None
    );
}

#[test]
fn remove_rect_kills_a_root_rect_and_double_remove_is_none() {
    headless_env();
    // `add_rect` needs a scene to attach to, which only exists once
    // `init_graphics` has run against a real backend — see `scene.rs`'s own
    // integration test for the same setup.
    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .init_graphics(&display, &backend)
        .expect("renderer, allocator and core globals");
    let rect = runtime.add_rect(4, 4, [0.0, 1.0, 0.0, 1.0]).expect("rect");
    assert_eq!(runtime.remove_rect(rect), Some(()));
    assert_eq!(runtime.remove_rect(rect), None);
    assert_eq!(
        runtime.set_rect_position(rect, 1, 1),
        None,
        "stale id must miss cleanly"
    );
}

#[test]
fn remove_fd_forgets_the_declaration() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let (r, _w) = std::io::pipe().expect("pipe");
    let id = runtime.add_fd(r.into(), wlr::Interest::READABLE);
    assert_eq!(runtime.remove_fd(id), Some(()));
    assert_eq!(runtime.remove_fd(id), None);
}

#[test]
fn removing_a_live_source_stops_its_callbacks() {
    // Registers two pipes, wakes both, removes one from inside its own
    // fd_ready, wakes both again, and asserts the removed source never
    // fires after removal while the surviving one does.
    headless_env();
    use std::io::Write;
    struct App {
        runtime: wlr::Runtime,
        doomed: wlr::SourceId,
        doomed_fires: u32,
        survivor_fires: u32,
        turns: u32,
    }
    impl wlr::OutputHandler for App {}
    impl wlr::ToplevelHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::SessionLockHandler for App {}
    impl wlr::FdHandler for App {
        fn fd_ready(
            &mut self,
            source: wlr::SourceId,
            fd: std::os::fd::BorrowedFd<'_>,
            _r: wlr::Readiness,
        ) {
            let mut buf = [0u8; 16];
            let _ = rustix::io::read(fd, &mut buf); // drain so level-triggering stops
            if source == self.doomed {
                self.doomed_fires += 1;
                assert_eq!(self.runtime.remove_fd(self.doomed), Some(()));
            } else {
                self.survivor_fires += 1;
            }
        }
    }
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns > 8
        }
    }

    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let (dr, mut dw) = std::io::pipe().expect("pipe");
    let (sr, mut sw) = std::io::pipe().expect("pipe");
    let doomed = runtime.add_fd(dr.into(), wlr::Interest::READABLE);
    let _survivor = runtime.add_fd(sr.into(), wlr::Interest::READABLE);
    dw.write_all(b"x").expect("wake doomed");
    sw.write_all(b"x").expect("wake survivor");
    let mut app = App {
        runtime: runtime.clone(),
        doomed,
        doomed_fires: 0,
        survivor_fires: 0,
        turns: 0,
    };
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(4))
        .expect("first run");
    // `doomed`'s read end was closed by the first run's `remove_fd` call
    // (its `OwnedFd` is owned by the declaration `remove_fd` drops), so
    // this write's only possible outcome is a broken pipe — nothing is
    // listening on the other end any more, and nothing should be. Ignored
    // rather than asserted on, unlike every other write in this test.
    let _ = dw.write_all(b"y");
    sw.write_all(b"y").expect("second wake survivor");
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(4))
        .expect("second run");
    // Weaker evidence than it looks, and deliberately kept anyway: the
    // second write above cannot reach `doomed` even if `remove_fd` had left
    // the source installed, because `remove_fd` also closed the read end, so
    // this pins "fires exactly once" without on its own proving *which* of
    // the two mechanisms stopped it. What it does prove — and what a
    // regression would break — is that the removal is idempotent from the
    // handler's side: the source does not re-fire within the four remaining
    // turns of the first run either, which a still-installed level-triggered
    // source would have done. The source-level evidence lives in
    // `remove_fd_forgets_the_declaration` (the second `remove_fd` reporting
    // `None`, there for a source that was never even run through `run_all`)
    // and in the surviving source below, which shows the loop itself kept
    // dispatching across both runs.
    assert_eq!(app.doomed_fires, 1, "removed source must not fire again");
    assert!(app.survivor_fires >= 2, "surviving source must keep firing");
}

#[test]
fn removed_fd_stays_valid_for_the_rest_of_its_own_callback() {
    // The C1 regression test: `remove_fd` must not close the descriptor
    // while the `BorrowedFd` this very `fd_ready` call was handed is still
    // live. Calls `remove_fd` *before* touching `fd` — unlike
    // `removing_a_live_source_stops_its_callbacks`, which happens to drain
    // first and so never exercises this ordering — and then reads through
    // the same `fd`, which must not fail with `EBADF` ("Bad file
    // descriptor"): that specific error is what a closed-out-from-under-it
    // descriptor produces.
    headless_env();
    use std::io::Write;
    struct App {
        runtime: wlr::Runtime,
        id: wlr::SourceId,
        after_remove: Option<Result<usize, rustix::io::Errno>>,
        turns: u32,
    }
    impl wlr::OutputHandler for App {}
    impl wlr::ToplevelHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::SessionLockHandler for App {}
    impl wlr::FdHandler for App {
        fn fd_ready(
            &mut self,
            source: wlr::SourceId,
            fd: std::os::fd::BorrowedFd<'_>,
            _r: wlr::Readiness,
        ) {
            assert_eq!(source, self.id);
            assert_eq!(self.runtime.remove_fd(self.id), Some(()));
            // `fd`'s borrow does not end until this call returns; the
            // descriptor it names must still be open right now.
            let mut buf = [0u8; 16];
            self.after_remove = Some(rustix::io::read(fd, &mut buf));
        }
    }
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.turns += 1;
            self.turns > 4
        }
    }

    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let (r, mut w) = std::io::pipe().expect("pipe");
    let id = runtime.add_fd(r.into(), wlr::Interest::READABLE);
    w.write_all(b"x").expect("wake");
    let mut app = App {
        runtime: runtime.clone(),
        id,
        after_remove: None,
        turns: 0,
    };
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Turns(4))
        .expect("run");

    match app.after_remove.expect("fd_ready must have fired") {
        Ok(n) => assert_eq!(
            n, 1,
            "the byte written before the run must still be readable"
        ),
        Err(e) => panic!(
            "reading `fd` after `remove_fd` failed with {e:?} — if this is \
             `BADF`, the descriptor was closed while a live `BorrowedFd` \
             still named it"
        ),
    }
}

#[test]
fn key_event_for_test_reports_what_it_was_given() {
    let ev = wlr::KeyEvent::for_test(
        0xff1b, /* XKB_KEY_Escape */
        wlr::Modifiers::default(),
        true,
        42,
    );
    assert_eq!(ev.keysym(), 0xff1b);
    assert!(ev.pressed());
    assert_eq!(ev.time_msec(), 42);
    assert!(!ev.modifiers().alt());
}

#[test]
fn toplevel_id_debug_and_dangling_bands_do_not_collide() {
    let a = wlr::ToplevelId::dangling_nth_for_test(1);
    let b = wlr::ToplevelId::dangling_nth_for_test(2);
    assert_ne!(a, b);
    assert!(!format!("{a:?}").is_empty());
}

/// `run_inner`'s retry-on-`EINTR` loop around `wl_event_loop_dispatch`
/// (`backend.rs`), covered by driving a real signal into the thread blocked
/// there.
///
/// Shape borrowed from `removing_a_live_source_stops_its_callbacks`: a
/// helper thread manipulates the world the main thread is blocked on, and
/// the assertion is made from the main thread once `run_all` returns. The
/// difference here is `Until::Stop` rather than `Until::Turns` — the retry
/// loop only matters when `wl_event_loop_dispatch` is actually blocked
/// (`Until::Turns`'s timeout is `0`, so it never blocks and the `EINTR`
/// branch is unreachable through it), so this test is the one place in the
/// suite that calls `run_all` with a blocking timeout.
///
/// Delivering a signal to *a specific OS thread* (the one inside the
/// blocking syscall, not whichever thread the kernel happens to pick for a
/// process-wide `kill`) needs `pthread_kill`/`pthread_self`, which are not
/// behind any feature `rustix` has enabled here (`pipe`, `std`) and are not
/// in `std`. Declared directly via `extern "C"` below rather than adding a
/// dependency for two function pointers.
///
/// Signal delivery timing is the one thing this test cannot pin down: the
/// signal can land before the main thread has entered
/// `wl_event_loop_dispatch`, in which case the retry branch simply is not
/// exercised on that run. That is why the helper thread *also*
/// unconditionally writes to the wakeup pipe a beat later — a real,
/// ordinary fd wakeup the loop was always going to see — so this test's
/// pass/fail does not depend on winning the race, only its power to catch a
/// regression does. What every run *does* prove is the acceptance
/// criterion in the review finding: `run_all` does not return `Err`, and
/// dispatch kept going (the fd wakeup after the signal still stops the
/// loop) — a `run_inner` that mapped `EINTR` straight to `Error::Operation`
/// instead of retrying would fail this test on any run that wins the race,
/// and CI runs it enough times that one eventually will.
#[test]
fn eintr_from_a_signal_during_a_blocking_run_all_does_not_fail_the_run() {
    headless_env();
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    type PthreadT = std::os::raw::c_ulong;
    const SIGUSR1: std::os::raw::c_int = 10;
    unsafe extern "C" {
        fn signal(
            signum: std::os::raw::c_int,
            handler: extern "C" fn(std::os::raw::c_int),
        ) -> usize;
        fn pthread_self() -> PthreadT;
        fn pthread_kill(thread: PthreadT, sig: std::os::raw::c_int) -> std::os::raw::c_int;
    }
    extern "C" fn noop_handler(_sig: std::os::raw::c_int) {}

    // SAFETY: installs a handler for `SIGUSR1`, a signal this test process
    // uses for nothing else, before the helper thread below (spawned only
    // after this call returns) can deliver one. Without this, `SIGUSR1`'s
    // default disposition terminates the process instead of interrupting
    // the blocked syscall with `EINTR`.
    unsafe {
        signal(SIGUSR1, noop_handler);
    }

    struct App {
        stop: AtomicBool,
    }
    impl wlr::OutputHandler for App {}
    impl wlr::ToplevelHandler for App {}
    impl wlr::SeatHandler for App {}
    impl wlr::SessionLockHandler for App {}
    impl wlr::FdHandler for App {
        fn fd_ready(
            &mut self,
            _source: wlr::SourceId,
            fd: std::os::fd::BorrowedFd<'_>,
            _r: wlr::Readiness,
        ) {
            let mut buf = [0u8; 16];
            let _ = rustix::io::read(fd, &mut buf);
            self.stop.store(true, Ordering::SeqCst);
        }
    }
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            self.stop.load(Ordering::SeqCst)
        }
    }

    let display = wlr::Display::new().expect("display");
    let runtime = wlr::Runtime::new().expect("runtime");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let (r, mut w) = std::io::pipe().expect("pipe");
    let _id = runtime.add_fd(r.into(), wlr::Interest::READABLE);

    // SAFETY: called on the main test thread, before the helper thread is
    // spawned, so this reads the identity of the thread that is about to
    // block in `run_all` below.
    let main_thread: PthreadT = unsafe { pthread_self() };
    // Records `pthread_kill`'s return value (`0` = delivered) so the
    // assertion below can tell a broken FFI declaration from a lost race —
    // both leave `run_all` returning `Ok`, but only the latter is this
    // test's actual claim.
    let kill_rc = AtomicI32::new(i32::MIN);

    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Long enough that the main thread has called `run_all` and
            // entered the blocking `wl_event_loop_dispatch` (`Until::Stop`'s
            // timeout is `-1`) before the signal is sent; see this test's
            // own doc for why landing early is harmless rather than a
            // source of flakiness.
            std::thread::sleep(std::time::Duration::from_millis(50));
            // SAFETY: `main_thread` was read from a live `pthread_self()`
            // call on that thread above, and the thread is still running
            // (blocked in `run_all`, which this scope's join waits out).
            let rc = unsafe { pthread_kill(main_thread, SIGUSR1) };
            kill_rc.store(rc, Ordering::SeqCst);
            // The unconditional, real wakeup: sent regardless of whether
            // the signal above landed mid-poll, so this test cannot hang
            // even on a lost race.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = w.write_all(b"x");
        });

        let mut app = App {
            stop: AtomicBool::new(false),
        };
        let result = backend.run_all(&display, &mut app, &runtime, wlr::Until::Stop);
        assert!(
            result.is_ok(),
            "a signal arriving during a blocking run_all must not surface \
             as Err — run_inner's EINTR retry must have swallowed it: {result:?}"
        );
    });

    assert_eq!(
        kill_rc.load(Ordering::SeqCst),
        0,
        "pthread_kill(main_thread, SIGUSR1) must have succeeded for this \
         run to be evidence of anything; a nonzero return means the retry \
         loop was never actually exercised"
    );
}
