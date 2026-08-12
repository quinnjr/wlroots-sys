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
    impl wlr::FdHandler for App {
        fn fd_ready(&mut self, source: wlr::SourceId, fd: std::os::fd::BorrowedFd<'_>, _r: wlr::Readiness) {
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
    // `removing_a_declared_but_unregistered_source_reports_once` (the second
    // `remove_fd` reporting `None`) and in the surviving source below, which
    // shows the loop itself kept dispatching across both runs.
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
    impl wlr::FdHandler for App {
        fn fd_ready(&mut self, source: wlr::SourceId, fd: std::os::fd::BorrowedFd<'_>, _r: wlr::Readiness) {
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
        Ok(n) => assert_eq!(n, 1, "the byte written before the run must still be readable"),
        Err(e) => panic!(
            "reading `fd` after `remove_fd` failed with {e:?} — if this is \
             `BADF`, the descriptor was closed while a live `BorrowedFd` \
             still named it"
        ),
    }
}

#[test]
fn key_event_for_test_reports_what_it_was_given() {
    let ev = wlr::KeyEvent::for_test(0xff1b /* XKB_KEY_Escape */, wlr::Modifiers::default(), true, 42);
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
