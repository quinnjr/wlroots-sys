//! `render/drm_syncobj.h`: timelines, waiters and the flags they take.
//!
//! # Why most of this skips rather than fails
//!
//! A `wlr_drm_syncobj_timeline` is a kernel object on a DRM device, so
//! everything below `flags_match_libdrm` needs a real `/dev/dri` node. CI runs
//! headless on the pixman renderer, which has no DRM device at all —
//! [`Renderer::drm_fd`](wlr::Renderer::drm_fd) answers `None` there by design —
//! so those tests return early with a note instead of failing. A machine with a
//! GPU runs them for real.
//!
//! Skipping is the honest option and not a weakening: the alternative is either
//! a test that fails on every CI run or one that quietly asserts nothing. The
//! flag-pinning test, which is the one that guards a value this crate had to
//! write out by hand, runs everywhere.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd};
use std::path::PathBuf;
use std::rc::Rc;

use wlr::{Display, Error, SyncFlags, SyncTimeline};

/// A render node to test against, or `None` on a machine without one.
///
/// Render nodes rather than card nodes: `renderD*` needs no DRM master and no
/// seat, which is what makes this work in an unprivileged container that does
/// happen to have a GPU.
fn drm_node() -> Option<File> {
    let dir = std::fs::read_dir("/dev/dri").ok()?;
    let mut nodes: Vec<PathBuf> = dir
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes
        .into_iter()
        .find_map(|path| File::options().read(true).write(true).open(path).ok())
}

/// Announce a skip on the machine's terms rather than the test's.
/// Report a test that could not run for want of a DRM render node.
///
/// Ten of the twelve tests in this file gate on one, and on a GPU-less runner
/// every one of them skips — so all of `render/sync.rs` goes unexercised while
/// the job reports green. A double `waiter_finish`, a leaked eventfd or a
/// stale `wl_event_source` in `SyncWaiter::Drop` would all merge without
/// anything noticing.
///
/// That cannot be fixed by asking harder: the hardware is either there or it
/// is not. What can be fixed is the silence. Setting `WLR_REQUIRE_DRM=1` turns
/// a skip into a failure, so a runner that is *supposed* to have a render node
/// says so when it stops having one — which is the regression that would
/// otherwise be invisible, because it looks exactly like success.
fn skip(what: &str) {
    if std::env::var_os("WLR_REQUIRE_DRM").is_some() {
        panic!(
            "{what} needs a /dev/dri render node and WLR_REQUIRE_DRM=1 says \
             this machine has one. Either the node disappeared or the variable \
             is set on a runner that never had one."
        );
    }
    eprintln!(
        "skipping {what}: no usable /dev/dri render node on this machine. \
         render/sync.rs is NOT covered by this run — set WLR_REQUIRE_DRM=1 on \
         a machine with a render node to make this a failure instead."
    );
}

/// Where libdrm's `drm.h` lives, according to pkg-config.
fn libdrm_header() -> Option<PathBuf> {
    let out = std::process::Command::new("pkg-config")
        .args(["--cflags-only-I", "libdrm"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let cflags = String::from_utf8(out.stdout).ok()?;
    cflags
        .split_whitespace()
        .filter_map(|flag| flag.strip_prefix("-I"))
        .map(|dir| PathBuf::from(dir).join("drm.h"))
        .find(|path| path.exists())
}

/// The `N` in `#define <name> (1 << N)`, read out of a C header.
fn defined_shift(header: &str, name: &str) -> Option<u32> {
    let line = header
        .lines()
        .find(|line| line.trim_start().starts_with(&format!("#define {name} ")))?;
    let (_, rest) = line.split_once("(1 <<")?;
    let (shift, _) = rest.split_once(')')?;
    shift.trim().parse().ok()
}

/// The one test here that runs everywhere, and the one that matters most.
///
/// `DRM_SYNCOBJ_WAIT_FLAGS_*` are libdrm's, not wlroots', so they are not in
/// `wlr-sys` — its allowlist is `wlr_.*`/`WLR_.*`. That leaves this crate
/// having written two integers out by hand, with nothing in the type system to
/// check them, and the values are **not** the obvious `1 << 0` / `1 << 1`:
/// `WAIT_ALL` occupies bit 0. Getting them wrong makes `check` answer about a
/// different question and `wait_for_timeline` fail with `EINVAL`.
///
/// So this reads the installed `drm.h` and compares. It skips only if libdrm's
/// headers are not installed, which on a machine that can build wlroots at all
/// they are.
#[test]
fn sync_flags_match_libdrms_own_header() {
    let Some(path) = libdrm_header() else {
        eprintln!("skipping flag pinning: pkg-config could not locate libdrm's drm.h");
        return;
    };
    let header = std::fs::read_to_string(&path).expect("read drm.h");

    let for_submit = defined_shift(&header, "DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT")
        .expect("DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT is defined as (1 << N)");
    let available = defined_shift(&header, "DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE")
        .expect("DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE is defined as (1 << N)");

    assert_eq!(
        SyncFlags::WAIT_FOR_SUBMIT.bits(),
        1 << for_submit,
        "{}: WAIT_FOR_SUBMIT is bit {for_submit}",
        path.display()
    );
    assert_eq!(
        SyncFlags::WAIT_AVAILABLE.bits(),
        1 << available,
        "{}: WAIT_AVAILABLE is bit {available}",
        path.display()
    );
    // Bit 0 is WAIT_ALL, which is meaningless for the single-handle waits
    // wlroots issues and is deliberately not exposed. Asserted so that a future
    // edit "correcting" WAIT_FOR_SUBMIT to 1 << 0 fails here.
    assert_ne!(SyncFlags::WAIT_FOR_SUBMIT.bits(), 1);
    assert_ne!(SyncFlags::WAIT_AVAILABLE.bits(), 1);
}

#[test]
fn a_timeline_reports_the_device_it_was_created_on() {
    let Some(node) = drm_node() else {
        return skip("timeline creation");
    };
    let timeline = SyncTimeline::create(node.as_fd()).expect("timeline");
    assert_eq!(
        timeline.drm_fd().as_raw_fd(),
        node.as_fd().as_raw_fd(),
        "the timeline keeps the descriptor it was handed"
    );
}

/// Signalling and checking are the two halves of a timeline's whole purpose.
///
/// `WAIT_FOR_SUBMIT` on the unsignalled point is not decoration: without it the
/// kernel rejects a point with no fence outright, and wlroots reports that as a
/// failed call rather than "not ready".
#[test]
fn signalling_a_point_makes_it_check_as_ready() {
    let Some(node) = drm_node() else {
        return skip("signal/check");
    };
    let timeline = SyncTimeline::create(node.as_fd()).expect("timeline");

    timeline.signal(1).expect("signal point 1");
    assert_eq!(timeline.check(1, SyncFlags::WAIT_FOR_SUBMIT), Ok(true));
    assert_eq!(
        timeline.check(2, SyncFlags::WAIT_FOR_SUBMIT),
        Ok(false),
        "nothing has signalled point 2"
    );

    timeline.signal(2).expect("signal point 2");
    assert_eq!(timeline.check(2, SyncFlags::WAIT_FOR_SUBMIT), Ok(true));
}

/// A timeline is reference-counted, so a clone keeps the kernel object alive
/// and observes the same points.
#[test]
fn a_cloned_timeline_is_the_same_timeline() {
    let Some(node) = drm_node() else {
        return skip("timeline refcounting");
    };
    let timeline = SyncTimeline::create(node.as_fd()).expect("timeline");
    let clone = timeline.clone();
    assert_eq!(clone.as_ptr(), timeline.as_ptr());

    clone.signal(7).expect("signal through the clone");
    drop(clone);

    assert_eq!(
        timeline.check(7, SyncFlags::WAIT_FOR_SUBMIT),
        Ok(true),
        "the original still names the object the clone signalled"
    );
}

/// Export hands over an owned descriptor and import turns one back into a
/// timeline — the pair a compositor uses to hand a timeline to a client.
#[test]
fn a_timeline_round_trips_through_an_exported_descriptor() {
    let Some(node) = drm_node() else {
        return skip("export/import");
    };
    let timeline = SyncTimeline::create(node.as_fd()).expect("timeline");
    timeline.signal(3).expect("signal point 3");

    let exported = timeline.export().expect("export");
    let imported = SyncTimeline::import(node.as_fd(), exported.as_fd()).expect("import");
    // A different `wlr_drm_syncobj_timeline`, the same kernel object.
    assert_ne!(imported.as_ptr(), timeline.as_ptr());
    assert_eq!(imported.check(3, SyncFlags::WAIT_FOR_SUBMIT), Ok(true));

    // The exported descriptor is the caller's: dropping it here must not
    // disturb the timeline that was imported from it.
    drop(exported);
    assert_eq!(imported.check(3, SyncFlags::WAIT_FOR_SUBMIT), Ok(true));
}

/// Transfer copies a fence between timelines on the same device.
#[test]
fn a_point_transfers_between_two_timelines_on_one_device() {
    let Some(node) = drm_node() else {
        return skip("transfer");
    };
    let src = SyncTimeline::create(node.as_fd()).expect("source timeline");
    let dst = SyncTimeline::create(node.as_fd()).expect("destination timeline");

    src.signal(5).expect("signal the source");
    dst.transfer(9, &src, 5).expect("transfer");
    assert_eq!(dst.check(9, SyncFlags::WAIT_FOR_SUBMIT), Ok(true));
}

/// `wlr_drm_syncobj_timeline_transfer` **asserts** both timelines are on the
/// same descriptor, and Arch's wlroots is built without `NDEBUG` — a failed
/// assertion aborts the process. This test passing is the evidence the
/// assertion cannot be reached from safe code.
#[test]
fn transferring_across_devices_is_refused_instead_of_aborting() {
    let Some(first) = drm_node() else {
        return skip("cross-device transfer");
    };
    // A second, independent descriptor for the same device is enough: wlroots
    // compares the descriptors, not the devices behind them.
    let Some(second) = drm_node() else {
        return skip("cross-device transfer");
    };
    assert_ne!(
        first.as_fd().as_raw_fd(),
        second.as_fd().as_raw_fd(),
        "two opens, two descriptors"
    );

    let src = SyncTimeline::create(first.as_fd()).expect("source timeline");
    let dst = SyncTimeline::create(second.as_fd()).expect("destination timeline");
    src.signal(1).expect("signal the source");

    assert_eq!(
        dst.transfer(1, &src, 1).unwrap_err(),
        Error::Mismatch("SyncTimeline::transfer")
    );
}

/// A sync file is the other export shape — a fence rather than a timeline —
/// and it round-trips back into a different point.
#[test]
fn a_sync_file_round_trips_into_another_point() {
    let Some(node) = drm_node() else {
        return skip("sync file export/import");
    };
    let timeline = SyncTimeline::create(node.as_fd()).expect("timeline");
    timeline.signal(1).expect("signal point 1");

    let sync_file = match timeline.export_sync_file(1) {
        Ok(fd) => fd,
        // Not every driver materialises a CPU-signalled point as an exportable
        // sync file; that is the kernel's business, not this wrapper's.
        Err(e) => {
            eprintln!("skipping sync file round trip: export_sync_file failed: {e}");
            return;
        }
    };
    timeline
        .import_sync_file(4, sync_file.as_fd())
        .expect("import the sync file into point 4");
    assert_eq!(timeline.check(4, SyncFlags::WAIT_FOR_SUBMIT), Ok(true));
}

/// The waiter's happy path: register, signal, dispatch, callback runs — and
/// then the waiter is dropped *after* it fired.
///
/// That last step is the one this test exists for.
/// `wlr_drm_syncobj_timeline_waiter_finish` is documented as "cancel", which
/// reads like it must not be called once the callback has run; the source says
/// otherwise (the callback path releases nothing), so `finish` is mandatory
/// either way. Both readings compile. Only one of them does not leak an
/// eventfd and an event source per wait, and the other would double-free.
#[test]
fn a_waiter_dropped_after_firing_neither_double_frees_nor_leaks() {
    let Some(node) = drm_node() else {
        return skip("timeline waiter");
    };
    let display = Display::new().expect("display");
    let event_loop = display.event_loop();
    let timeline = SyncTimeline::create(node.as_fd()).expect("timeline");

    let fired = Rc::new(Cell::new(false));
    let flag = Rc::clone(&fired);
    let waiter = event_loop
        .wait_for_timeline(&timeline, 1, SyncFlags::NONE, move || flag.set(true))
        .expect("register the wait");
    assert!(!waiter.fired());

    timeline.signal(1).expect("signal point 1");

    // The eventfd is signalled by the kernel, so give the loop a few turns
    // rather than assuming one dispatch is enough.
    for _ in 0..20 {
        event_loop.dispatch(50).expect("dispatch");
        if fired.get() {
            break;
        }
    }
    assert!(fired.get(), "the callback did not run");
    assert!(waiter.fired());

    // The whole point: dropping a waiter whose callback already ran runs
    // `finish`, which must be legal.
    drop(waiter);

    // The loop still works afterwards, which a corrupted source list would not
    // survive.
    event_loop.dispatch(0).expect("dispatch after the drop");
}

/// The other half: a waiter dropped *before* its point ever signals must
/// cancel cleanly, and the callback must never run afterwards.
#[test]
fn a_waiter_dropped_before_firing_cancels_cleanly() {
    let Some(node) = drm_node() else {
        return skip("timeline waiter cancellation");
    };
    let display = Display::new().expect("display");
    let event_loop = display.event_loop();
    let timeline = SyncTimeline::create(node.as_fd()).expect("timeline");

    let fired = Rc::new(Cell::new(false));
    let flag = Rc::clone(&fired);
    let waiter = event_loop
        .wait_for_timeline(&timeline, 100, SyncFlags::NONE, move || flag.set(true))
        .expect("register the wait");
    assert!(!waiter.fired());
    drop(waiter);

    // Signal the point the abandoned waiter was watching. Its event source is
    // gone, so nothing may be delivered — a stale source here would call
    // through a freed box.
    timeline.signal(100).expect("signal point 100");
    for _ in 0..5 {
        event_loop.dispatch(10).expect("dispatch");
    }
    assert!(!fired.get(), "a cancelled wait must not fire");
}

/// The callback runs inside `wl_event_loop_dispatch`, so it is a handler frame:
/// driving the loop again from inside it would let wlroots free objects an
/// enclosing frame still holds. `EventLoop::dispatch` refuses while any handler
/// is on the stack, and this pins that the waiter's callback counts as one.
#[test]
fn a_waiters_callback_is_a_handler_frame_and_cannot_drive_the_loop() {
    let Some(node) = drm_node() else {
        return skip("waiter reentrancy");
    };
    let display = Display::new().expect("display");
    let event_loop = display.event_loop();
    let timeline = SyncTimeline::create(node.as_fd()).expect("timeline");

    let refused: Rc<RefCell<Option<Result<(), Error>>>> = Rc::new(RefCell::new(None));
    let slot = Rc::clone(&refused);
    // The callback re-derives the loop from a display it captured, which is the
    // path a compositor's own state makes available and which no lifetime can
    // close off.
    let waiter = event_loop
        .wait_for_timeline(&timeline, 1, SyncFlags::NONE, move || {
            let display = Display::new().expect("display");
            *slot.borrow_mut() = Some(display.event_loop().dispatch(0));
        })
        .expect("register the wait");

    timeline.signal(1).expect("signal point 1");
    for _ in 0..20 {
        event_loop.dispatch(50).expect("dispatch");
        if refused.borrow().is_some() {
            break;
        }
    }
    drop(waiter);

    assert_eq!(
        *refused.borrow(),
        Some(Err(Error::Reentrant("EventLoop::dispatch"))),
        "a waiter callback must not be able to drive an event loop"
    );

    // And the flag is restored on the way out, or the loop would be wedged shut
    // for the rest of the thread's life.
    event_loop.dispatch(0).expect("dispatch after the callback");
}

/// A waiter borrows the loop, so it cannot outlive the display that owns it —
/// `wl_event_source_remove` on a destroyed loop is a use-after-free. The
/// compile-fail half of this claim is `tests/ui/sync_waiter_outlives_display.rs`.
#[test]
fn a_waiter_and_a_timeline_stay_on_one_thread() {
    use static_assertions::assert_not_impl_any;

    assert_not_impl_any!(SyncTimeline: Send, Sync);
    assert_not_impl_any!(wlr::SyncWaiter<'static>: Send, Sync);
}
