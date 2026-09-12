//! M7 cursor depth: snapshot readers miss cleanly with no seat.

use wlr::CursorId;

/// The headless backend is selected by environment, read once when the
/// display and backend are created below.
fn headless_env() {
    // SAFETY: libtest runs the tests in a binary in parallel by default, so
    // this is *not* safe by virtue of serial execution. It is safe because all
    // tests in this binary set the same two variables to the same values (and
    // no `#[test]` here spawns threads that read the environment): a
    // concurrent write from another test writes byte-identical values, so no
    // harness thread can observe a torn environment read.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }
}

#[test]
fn dangling_cursor_misses_cleanly() {
    headless_env();
    let rt = wlr::Runtime::new().unwrap();
    assert!(rt.cursor_state().is_none());
    assert!(rt.try_cursor(CursorId::dangling()).is_none());
}

#[test]
fn cursor_map_misses_without_seat() {
    headless_env();
    let rt = wlr::Runtime::new().unwrap();
    assert!(
        rt.map_cursor_to_region(wlr::Box2D::new(0, 0, 100, 100))
            .is_none(),
        "no seat cursor, so no mapping applies"
    );
}

#[test]
fn cursor_appears_with_seat() {
    use wlr::CursorImage;

    headless_env();
    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let rt = wlr::Runtime::new().expect("runtime");
    rt.init_graphics(&display, &backend).expect("graphics");

    assert!(rt.cursor_id().is_none(), "no seat yet, so no cursor id");

    rt.create_seat(&display, "seat0").expect("seat");

    let id = rt.cursor_id().expect("seat brings a cursor");
    let state = rt.cursor_state().expect("tracked cursor snapshots");
    assert_eq!(
        state,
        rt.try_cursor(id).expect("id resolves to the same state")
    );
    // The position is whatever wlroots initialised the cursor to — the
    // snapshot reports the live value, which is what `pointer_position`
    // reads from the same cursor, not a constant this test invents.
    assert_eq!(state.position, rt.pointer_position());
    assert_eq!(state.mapped_output, None);
    assert_eq!(state.mapped_region, None);
    assert_eq!(state.image, CursorImage::Hidden);
    assert_eq!(state.hotspot, (0, 0));
    assert!(rt.try_cursor(CursorId::dangling()).is_none());
}

#[test]
fn xcursor_theme_destroy_roundtrip() {
    use wlr::XcursorManagerId;

    headless_env();
    let rt = wlr::Runtime::new().unwrap();

    // Unknown ids are harmless no-ops, never double-frees.
    assert!(!rt.destroy_xcursor_theme(XcursorManagerId::dangling()));

    // Whether this container has themes installed decides `Some` vs `None`
    // here, so both outcomes are accepted — but a loaded theme must round
    // trip through destroy exactly once.
    if let Some(id) = rt.load_xcursor_theme("default", 24) {
        assert!(rt.destroy_xcursor_theme(id), "first destroy releases");
        assert!(!rt.destroy_xcursor_theme(id), "second destroy misses");
    }
    // A NUL in the name can never load.
    assert!(rt.load_xcursor_theme("def\0ault", 24).is_none());
}
