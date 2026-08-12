//! RGBA pixel-buffer scene nodes, against a real headless compositor with no
//! client — the same shape `scene.rs`'s tests use for rects.

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See
/// `toplevels.rs`'s sibling copy of this helper for the full argument for
/// why this is a `Once` rather than a plain `set_var` per test.
fn headless_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once::call_once` runs this closure at most once and
        // blocks every other caller of `call_once` on this `Once` until it
        // returns, so no concurrent `getenv` from another test's call to
        // `headless_env` can observe a torn write.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
        }
    });
}

/// A runtime with graphics initialised, so `add_buffer` has a scene to
/// attach to — `add_buffer` mirrors `add_rect`'s "no scene, no node" rule
/// (see `runtime.rs`'s own doc on both), so unlike the validation-only test
/// below, a lifecycle test that expects `add_buffer` to actually succeed
/// needs a real backend and `init_graphics`, not just `Runtime::new`.
fn graphics_runtime() -> wlr::Runtime {
    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .init_graphics(&display, &backend)
        .expect("graphics");
    // Leaked deliberately: this helper's only job is handing back a runtime
    // with a live scene to attach buffer nodes to, and `Runtime`'s own
    // lifetime obligation (see its doc) is that it must not outlive the
    // `Display` it was initialised against. Leaking `display` and `backend`
    // here — rather than letting them drop at the end of this function —
    // is what keeps that obligation satisfied for the rest of each test.
    std::mem::forget(backend);
    std::mem::forget(display);
    runtime
}

#[test]
fn buffer_node_lifecycle_by_id() {
    headless_env();
    let runtime = graphics_runtime();
    let px = vec![0u8; 8 * 8 * 4];
    let id = runtime.add_buffer(8, 8, &px).expect("add");
    assert_eq!(runtime.set_buffer_position(id, 5, 7), Some(()));
    assert_eq!(runtime.set_buffer_dest_size(id, 64, 64), Some(()));
    assert_eq!(
        runtime.update_buffer(id, 4, 4, &[255u8; 4 * 4 * 4]),
        Some(())
    );
    assert_eq!(runtime.lower_buffer_to_bottom(id), Some(()));
    assert_eq!(runtime.remove_buffer(id), Some(()));
    assert_eq!(runtime.remove_buffer(id), None);
    assert_eq!(runtime.set_buffer_position(id, 0, 0), None);
}

#[test]
fn add_buffer_rejects_wrong_length_and_bad_dimensions() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    assert!(runtime.add_buffer(8, 8, &[0u8; 4]).is_err());
    assert!(runtime.add_buffer(0, 8, &[]).is_err());
    assert!(runtime.add_buffer(-1, 8, &[]).is_err());
}

#[test]
fn in_toplevel_buffer_on_a_dead_id_is_none() {
    headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    let dead = wlr::ToplevelId::dangling_for_test();
    assert_eq!(runtime.add_buffer_in_toplevel(dead, 2, 2, &[0u8; 16]), None);
}
