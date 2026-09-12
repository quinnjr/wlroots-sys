//! Output signal events, against a real headless backend.
//!
//! The setup itself is `common::headless_env`, shared with the other output
//! test binaries: this
//! integration binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local). Handler observations are recorded on `App`
//! and asserted after the run — never inside a handler, where a panic
//! would abort through C.
//!
//! Commit and precommit fire from in-harness commits; damage fires from
//! moving a software cursor (per the signal's own doc: "software cursors
//! or backend-specific logic" — staged commit damage does NOT emit it).
//! Cursor damage flushes the cursor's current position rather than one
//! event per move, so moves are not independent stimuli: the union rule
//! for coalesced emissions is pinned by the `union_into` unit test in
//! `backend.rs`, and this binary proves delivery carries the live damage.
//! Bind and request-state need a Wayland client (binding the output
//! global, speaking output-management), which no harness binary has —
//! they are wired and ledger-claimed, but e2e-only (icedtea harness — no
//! wlr milestone, the gap is environmental, not API).

use wlr::{
    Allocator, Backend, Box2D, CommittedFields, Display, Output, OwnedBuffer, Region, Renderer,
    Runtime, Until,
};

mod common;
#[path = "common/format.rs"]
mod format;
use common::headless_env;
use format::argb;

/// One commit-family delivery, in arrival order. Recording the sequence
/// (not just the last of each kind) is what makes a swapped
/// commit/precommit wiring fail: transposed callbacks still produce equal
/// masks, but never in this order with both timestamps set.
#[derive(Debug)]
enum CommitEv {
    Pre(CommittedFields, std::time::Duration),
    Commit(CommittedFields, std::time::Duration),
}

#[derive(Default)]
struct Observed {
    setup_commit_ok: Option<bool>,
    cursor_moved: Option<bool>,
    /// Every commit-family event, in delivery order.
    commit_log: Vec<CommitEv>,
    /// Every damage delivery's extents, in delivery order.
    damaged: Vec<Box2D>,
}

struct App<'a> {
    cursor_buf: OwnedBuffer<'a>,
    runtime: Runtime,
    /// When still, no cursor is shown: no `output_damaged` may fire.
    /// Commits still emit (wlroots signals empty regions), which the
    /// dispatch filters before delivery.
    cursor: Cursor,
    seen: Observed,
}

/// Whether the harness moves the software cursor. A plain `bool` at the
/// `run_app` boundary reads as blindness at both call sites (`true`/`false`
/// with no meaning), so the two cases are named.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cursor {
    Moved,
    Still,
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

        // A single cursor move to a known hotspot. Needs render init
        // first, as the cursor test in `output.rs` documents.
        if self.cursor == Cursor::Moved && self.runtime.init_output(output).is_ok() {
            let buf: &wlr::Buffer = &self.cursor_buf;
            if let Some(mut cursor) = output.create_cursor() {
                cursor.set_buffer(buf, 1, 1);
                self.seen.cursor_moved = Some(cursor.move_to(10.0, 20.0));
                cursor.destroy();
            }
        }
    }

    fn output_precommitted(
        &mut self,
        _output: &Output<'_>,
        fields: CommittedFields,
        when: std::time::Duration,
    ) {
        self.seen.commit_log.push(CommitEv::Pre(fields, when));
    }

    fn output_committed(
        &mut self,
        _output: &Output<'_>,
        fields: CommittedFields,
        when: std::time::Duration,
    ) {
        self.seen.commit_log.push(CommitEv::Commit(fields, when));
    }

    fn output_damaged(&mut self, _output: &Output<'_>, damage: Region) {
        self.seen.damaged.push(damage.extents());
    }
}

impl wlr::ToplevelHandler for App<'_> {}
impl wlr::SeatHandler for App<'_> {}
impl wlr::FdHandler for App<'_> {}
impl wlr::LoopHandler for App<'_> {}

/// Whether any damage delivery covers `(x, y)` with at least the 8x8 cursor
/// image: the box must contain the point and be at least 8x8.
fn covers_hotspot(delivered: &[Box2D], x: i32, y: i32) -> bool {
    delivered.iter().any(|b| {
        b.width >= 8
            && b.height >= 8
            && b.x <= x
            && b.y <= y
            && b.x + b.width >= x
            && b.y + b.height >= y
    })
}

fn run_app(cursor: Cursor) -> Observed {
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
        cursor,
        seen: Observed::default(),
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    app.seen
}

/// Committing fires precommit then commit with the staged mask and live
/// timestamps, and the cursor move delivers damage covering its hotspot.
/// Histories (not last-write slots) so extra backend commits on a future
/// wlroots cannot flake this: the staged transaction must appear in order,
/// whatever else fires around it.
#[test]
fn output_signal_events_fire_with_staged_payloads() {
    let seen = run_app(Cursor::Moved);
    assert_eq!(
        seen.setup_commit_ok,
        Some(true),
        "setup commit must succeed on headless"
    );
    let staged = CommittedFields::ENABLED | CommittedFields::MODE;
    // Precommit-with-staged must precede commit-with-staged: swapped
    // callbacks produce the same two masks in the wrong order and fail here.
    let pre_idx = seen
        .commit_log
        .iter()
        .position(|ev| matches!(ev, CommitEv::Pre(f, w) if *f == staged && !w.is_zero()));
    let commit_idx = seen
        .commit_log
        .iter()
        .position(|ev| matches!(ev, CommitEv::Commit(f, w) if *f == staged && !w.is_zero()));
    assert!(
        matches!((pre_idx, commit_idx), (Some(p), Some(c)) if p < c),
        "precommit-with-staged must precede commit-with-staged, both timestamped; got {:?}",
        seen.commit_log
    );
    assert_eq!(
        seen.cursor_moved,
        Some(true),
        "cursor move must succeed for damage to mean anything"
    );
    assert!(
        covers_hotspot(&seen.damaged, 10, 20),
        "a delivery must cover the cursor hotspot (10, 20) with at least the 8x8 image, got {:?}",
        seen.damaged
    );
}

/// No cursor movement means no damage delivery: the setup commit's empty
/// damage emission is filtered before delivery rather than waking the
/// compositor for an empty region.
#[test]
fn output_damage_stays_quiet_without_cursor_movement() {
    let seen = run_app(Cursor::Still);
    assert_eq!(
        seen.setup_commit_ok,
        Some(true),
        "setup commit must succeed on headless"
    );
    assert!(
        seen.damaged.is_empty(),
        "no cursor movement must mean no damage delivered, got {:?}",
        seen.damaged
    );
}

/// The presentation global constructs once and refuses twice. Matching on
/// the error (not just `is_err`) is what pins the guard rather than a
/// canned refusal: remove the double-create check and this fails.
#[test]
fn presentation_global_constructs_once() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    runtime
        .create_presentation(&display, &backend)
        .expect("presentation creates on a live backend");
    assert!(
        matches!(
            runtime.create_presentation(&display, &backend),
            Err(wlr::Error::Operation(_))
        ),
        "second presentation create must refuse as a double-create"
    );
}

/// The tearing-control global constructs once and refuses twice, same terms
/// as presentation above.
#[test]
fn tearing_control_global_constructs_once() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    runtime
        .create_tearing_control_manager(&display, 1)
        .expect("tearing control creates");
    assert!(
        matches!(
            runtime.create_tearing_control_manager(&display, 1),
            Err(wlr::Error::Operation(_))
        ),
        "second tearing control create must refuse as a double-create"
    );
}
