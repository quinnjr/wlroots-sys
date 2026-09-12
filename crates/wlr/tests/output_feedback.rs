//! Output signal events, against a real headless backend.
//!
//! Same shape as the other per-binary `headless_env` helpers: this
//! integration binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local). Handler observations are recorded on `App`
//! and asserted after the run — never inside a handler, where a panic
//! would abort through C.
//!
//! Commit and precommit fire from an in-harness commit; damage fires from
//! moving a software cursor (per the signal's own doc: "software cursors
//! or backend-specific logic"). Bind and request-state need a Wayland
//! client (binding the output global, speaking output-management), which
//! no harness binary has — they are wired and ledger-claimed, but e2e-only
//! (icedtea harness).

use std::sync::Once;
use wlr::{
    Allocator, Backend, Box2D, CommittedFields, Display, Output, OwnedBuffer, Region, Renderer,
    Runtime, Until,
};

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See `output.rs`'s
/// identical copy for the full argument — this is a separate integration-test
/// binary with its own environment.
fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once::call_once` runs this closure at most once and blocks
        // every other caller on this `Once` until it returns, so no concurrent
        // `getenv` can observe a torn write.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    });
}

/// The linear ARGB8888 format cursor buffers are allocated in — the same
/// choice as `tests/output.rs`'s `argb()`, so the pixman allocator hands out
/// mappable buffers a cursor can take.
fn argb() -> wlr::DrmFormat {
    wlr::DrmFormat::new(wlr::FourCc::ARGB8888, [wlr::Modifier::LINEAR])
}

#[derive(Default)]
struct Observed {
    setup_commit_ok: Option<bool>,
    cursor_moved: Option<bool>,
    precommit_fields: Option<CommittedFields>,
    committed_fields: Option<CommittedFields>,
    committed_when: Option<std::time::Duration>,
    damaged_extents: Option<Box2D>,
}

struct App<'a> {
    cursor_buf: OwnedBuffer<'a>,
    runtime: Runtime,
    seen: Observed,
}

impl wlr::OutputHandler for App<'_> {
    fn new_output(&mut self, output: &Output<'_>) {
        // One atomic transaction: enable + mode. The commit must emit
        // precommit first (staged mask), then commit (same mask). Recorded,
        // never asserted: a panic here would abort through C.
        let mut st = output.state();
        st.set_enabled(true);
        st.set_custom_mode(800, 600, 60_000);
        self.seen.setup_commit_ok = Some(st.commit().is_ok());

        // Moving a software cursor damages the output, which delivers
        // `output_damaged` below. Needs render init first, as the cursor
        // test in `output.rs` documents.
        if self.runtime.init_output(output).is_ok() {
            let buf: &wlr::Buffer = &self.cursor_buf;
            if let Some(mut cursor) = output.create_cursor() {
                cursor.set_buffer(buf, 1, 1);
                self.seen.cursor_moved = Some(cursor.move_to(10.0, 20.0));
                cursor.destroy();
            }
        }
    }

    fn output_precommit(
        &mut self,
        _output: &Output<'_>,
        fields: CommittedFields,
        _when: std::time::Duration,
    ) {
        self.seen.precommit_fields = Some(fields);
    }

    fn output_committed(
        &mut self,
        _output: &Output<'_>,
        fields: CommittedFields,
        when: std::time::Duration,
    ) {
        self.seen.committed_fields = Some(fields);
        self.seen.committed_when = Some(when);
    }

    fn output_damaged(&mut self, _output: &Output<'_>, damage: Region) {
        self.seen.damaged_extents = Some(damage.extents());
    }
}

impl wlr::ToplevelHandler for App<'_> {}
impl wlr::SeatHandler for App<'_> {}
impl wlr::FdHandler for App<'_> {}
impl wlr::LoopHandler for App<'_> {}

/// Committing fires precommit then commit with the staged mask, and moving
/// the software cursor delivers non-empty damage. The default `LoopHandler`
/// never stops early, so `Until::Turns` bounds the run.
#[test]
fn output_signal_events_fire_with_staged_payloads() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    // The cursor image, built exactly as `tests/output.rs`'s cursor test
    // does: a pixman renderer plus `Allocator::autocreate` on this backend,
    // then an 8x8 linear ARGB8888 buffer.
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let cursor_buf = allocator.create_buffer(8, 8, &argb()).expect("buffer");

    let mut app = App {
        cursor_buf,
        runtime: runtime.clone(),
        seen: Observed::default(),
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");

    let seen = app.seen;
    assert_eq!(
        seen.setup_commit_ok,
        Some(true),
        "setup commit must succeed on headless"
    );
    let staged = CommittedFields::ENABLED | CommittedFields::MODE;
    assert_eq!(
        seen.precommit_fields,
        Some(staged),
        "precommit must carry the staged mask before the commit applies"
    );
    assert_eq!(
        seen.committed_fields,
        Some(staged),
        "commit must carry the same staged mask after applying"
    );
    assert!(
        seen.committed_when.is_some_and(|w| !w.is_zero()),
        "commit carries wlroots' timestamp, which a clock produced and so is non-zero"
    );
    assert_eq!(
        seen.cursor_moved,
        Some(true),
        "cursor move must succeed for damage to mean anything"
    );
    let extents = seen.damaged_extents.expect("damage must be delivered");
    assert!(
        extents.width > 0 && extents.height > 0,
        "delivered damage must be non-empty, got {extents:?}"
    );
}
