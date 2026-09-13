//! Per-surface presentation feedback and tearing hints, against a real headless
//! backend.
//!
//! The positive paths need a connected Wayland client: `wp_presentation`
//! feedback exists only once a client has asked for it, and a tearing-control
//! object exists only once a client has created one. This binary owns the
//! environment (`Display` + `Backend` + `Runtime` + `init_graphics`, keeping
//! `display` a live local) and pins what is reachable without a client: the
//! by-id lookup/miss contract, which is the memory-safety boundary a
//! client-driven path would later cross.
//!
//! Handler observations are never asserted here; nothing in these tests runs
//! underneath an `extern "C"` frame.

use wlr::{Backend, Display, Runtime, SurfaceId};

mod common;

/// Every new per-surface operation misses cleanly on an unknown id, before and
/// after the globals exist. Removing the id-table lookup (dereferencing the id as
/// a pointer) would segfault here rather than return `None`.
#[test]
fn surface_presentation_and_tearing_ops_miss_cleanly_without_a_surface() {
    let _serial = common::headless_guard();
    common::headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    // Before either global exists: no surface can have requested feedback or
    // created a tearing control, so every lookup is a miss.
    assert!(
        runtime
            .sample_presentation(SurfaceId::dangling_for_test())
            .is_none(),
        "a dangling surface id has no presentation feedback"
    );
    assert!(
        runtime
            .tearing_hint(SurfaceId::dangling_for_test())
            .is_none(),
        "a dangling surface id has no tearing hint"
    );
    assert!(
        runtime
            .tearing_control(SurfaceId::dangling_for_test())
            .is_none(),
        "a dangling surface id has no tearing control object"
    );

    runtime
        .create_presentation(&display, &backend)
        .expect("presentation creates");
    runtime
        .create_tearing_control_manager(&display, 1)
        .expect("tearing control creates");

    // The globals now exist, but a dangling id still names no surface, so the
    // miss is unchanged — the lookup resolves the id before it reaches wlroots.
    assert!(
        runtime
            .sample_presentation(SurfaceId::dangling_nth_for_test(1))
            .is_none(),
        "the presentation global alone does not make a stale id resolve"
    );
    assert!(
        runtime
            .tearing_hint(SurfaceId::dangling_nth_for_test(1))
            .is_none(),
        "the tearing global alone does not make a stale id resolve"
    );
    assert!(
        runtime
            .tearing_control(SurfaceId::dangling_nth_for_test(1))
            .is_none(),
        "the tearing global alone does not make a stale id resolve"
    );
}
