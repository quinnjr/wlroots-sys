//! Criterion benches for the safe layer's per-operation overhead.
//!
//! Each group runs the same work twice: once through a `wlr` wrapper and once
//! through the raw `wlr_sys` binding it wraps. The difference is the safe
//! layer's cost, not wlroots'. `wlr`'s own `sys` module is private, so the raw
//! half goes through `wlr_sys`, a normal dependency Cargo makes available to
//! this target.
//!
//! One headless runtime is brought up lazily, outside every timed loop (see
//! [`HARNESS`]). It uses the same `WLR_BACKENDS=headless` / pixman environment
//! the integration tests set, because a bench that picked the developer's real
//! session would measure the machine instead of this crate.

use std::cell::OnceCell;
use std::ffi::c_void;

use criterion::{Criterion, black_box, criterion_group, criterion_main};

// The raw half of every pair. `wlr-sys` is a normal dependency of `wlr`, which
// Cargo exposes to bench targets; `wlr::sys` is deliberately private.
use wlr_sys as sys;

/// A headless display, backend and scene, live for the whole process.
///
/// `!Send`/`!Sync`, so it lives in thread-local storage rather than a `static`:
/// the handles are event-loop-bound and one thread owns them for the run.
struct Harness {
    /// Declared before the backend so it drops first: `init_graphics` requires
    /// the runtime not to outlive the display, and the backend's listeners must
    /// not outlive the loop they are linked on.
    runtime: wlr::Runtime,
    _backend: wlr::Backend<'static>,

    /// One rect's node, from both sides of the wrapper.
    node: wlr::NodeId,
    node_ptr: *mut sys::wlr_scene_node,
    rect_ptr: *mut sys::wlr_scene_rect,
}

thread_local! {
    static HARNESS: OnceCell<Harness> = const { OnceCell::new() };
}

impl Harness {
    fn new() -> Harness {
        // SAFETY: this runs once, on this thread, before any libwayland or
        // wlroots call, and no other thread reads the environment concurrently.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");
        }

        // Leaked deliberately: `Backend<'d>` borrows the `EventLoop` it was
        // created from, and a self-referential harness cannot own both. The
        // process exits at the end of the run, so the leak is bounded.
        let display: &'static wlr::Display =
            Box::leak(Box::new(wlr::Display::new().expect("headless display")));
        let event_loop: &'static wlr::EventLoop<'static> =
            Box::leak(Box::new(display.event_loop()));
        let backend = wlr::Backend::autocreate(event_loop).expect("headless backend");
        let runtime = wlr::Runtime::new().expect("runtime");
        runtime
            .init_graphics(display, &backend)
            .expect("renderer, allocator and core globals");

        let rect = runtime
            .add_rect(64, 64, [0.1, 0.1, 0.12, 1.0])
            .expect("scene rect");
        let node = runtime.rect_node(rect).expect("rect node id");
        // Captured through the wrapper once; the raw halves dereference these.
        let node_ptr = runtime
            .with_node(node, |n| n.as_ptr())
            .expect("node handle");
        let rect_ptr = runtime
            .with_rect(node, |r| r.as_ptr())
            .expect("rect handle");

        Harness {
            runtime,
            _backend: backend,
            node,
            node_ptr,
            rect_ptr,
        }
    }
}

/// Resolve a node handle by id, then read it — against reading the pointer.
///
/// The safe side pays the id-table lookup, the liveness check, the node-borrow
/// guard and the foreign-frame flag `Runtime::with_node` sets; the raw side
/// pays a pointer dereference.
fn handle_borrow(c: &mut Criterion) {
    HARNESS.with(|cell| {
        let h = cell.get_or_init(Harness::new);

        c.bench_function("handle_borrow/safe", |b| {
            b.iter(|| {
                h.runtime
                    .with_node(black_box(h.node), |n| black_box(n.position()))
            });
        });

        c.bench_function("handle_borrow/raw", |b| {
            // SAFETY: `node_ptr` names a live node for the process's life; the
            // harness holds the runtime that owns it.
            b.iter(|| unsafe { black_box(((*h.node_ptr).x, (*h.node_ptr).y)) });
        });
    });
}

/// A sink for listener callbacks; the write is the observable work.
unsafe extern "C" fn bump(_listener: *mut sys::wl_listener, data: *mut c_void) {
    if let Some(cell) = unsafe { (data as *mut u64).as_mut() } {
        *cell = cell.wrapping_add(1);
    }
}

/// One event through the observer layer — `wl_signal_emit_mutable` over a
/// `wl_listener` linked with `wl_signal_add` — against invoking a bare
/// listener's `notify` directly.
///
/// The `wlr` crate's own dispatcher and its `Registration` listener wrapper are
/// `pub(crate)`, so an external bench target cannot drive them; this compares
/// the signal-machinery primitives the wrapper is built on with the minimum
/// cost of delivering the same event.
fn listener_dispatch(c: &mut Criterion) {
    // An unlinked list head. Both the signal and the listeners are fully
    // initialised: a zeroed `wl_listener` is UB (its `notify` must be non-null),
    // so the fields are written explicitly instead.
    let unlinked = || sys::wl_list {
        prev: std::ptr::null_mut(),
        next: std::ptr::null_mut(),
    };

    // A real `wl_signal` with one listener linked, as the crate links its own.
    let mut signal = sys::wl_signal {
        listener_list: unlinked(),
    };
    // SAFETY: `signal` is a live local with an initialised list head.
    unsafe { sys::wl_signal_init(&raw mut signal) };
    let mut listener = sys::wl_listener {
        link: unlinked(),
        notify: bump,
    };
    // SAFETY: `signal` and `listener` are live locals for the whole function.
    unsafe { sys::wl_signal_add(&raw mut signal, &raw mut listener) };

    // The bare listener is never linked; the bench calls its callback itself.
    let mut bare = sys::wl_listener {
        link: unlinked(),
        notify: bump,
    };

    let mut data: u64 = 0;
    let data_ptr = &raw mut data as *mut c_void;

    c.bench_function("listener_dispatch/observer", |b| {
        b.iter(|| {
            // SAFETY: `signal` is initialised and its only listener is live
            // with a valid `notify`; `data` outlives the call and is the type
            // the callback expects.
            unsafe { sys::wl_signal_emit_mutable(&raw mut signal, data_ptr) };
            black_box(data)
        });
    });

    c.bench_function("listener_dispatch/raw", |b| {
        b.iter(|| {
            // SAFETY: `bare` is a live listener with a `notify` callback that
            // only touches `data`, which outlives the call.
            unsafe { (bare.notify)(&raw mut bare, data_ptr) };
            black_box(data)
        });
    });

    // Unlink before the stack frame goes away, so nothing later walks a list
    // whose head no longer exists.
    unsafe { sys::wayland_sys::server::wl_list_remove(&raw mut listener.link) };
}

/// Recolour a rect node through the wrapper against the raw `wlr_sys` call.
///
/// The wrapper resolves the node id, checks it is an owned node, raises the
/// borrow gate and only then reaches wlroots; the raw side is the wlroots call
/// alone.
fn scene_op(c: &mut Criterion) {
    HARNESS.with(|cell| {
        let h = cell.get_or_init(Harness::new);
        let color = [0.2, 0.2, 0.25, 1.0];

        c.bench_function("scene_op/safe", |b| {
            b.iter(|| {
                h.runtime
                    .set_node_rect_color(black_box(h.node), black_box(color))
            });
        });

        c.bench_function("scene_op/raw", |b| {
            // SAFETY: `rect_ptr` names a live rect for the process's life, and
            // `color` is a live four-float array for the call, which wlroots
            // copies.
            b.iter(|| unsafe {
                sys::wlr_scene_rect_set_color(h.rect_ptr, color.as_ptr());
            });
        });
    });
}

criterion_group!(benches, handle_borrow, listener_dispatch, scene_op);
criterion_main!(benches);
