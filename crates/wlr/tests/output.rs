//! Output atomic state and accessors, against a real headless backend.
//!
//! This integration binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local); the setup itself is `common::headless_env`,
//! shared with the other output test binaries. Handler observations are
//! recorded on `App` and asserted after the run — never inside a handler,
//! where a panic would abort through C.

mod common;
#[path = "common/format.rs"]
mod format;

use common::headless_env;
use format::argb;
use wlr::{
    Allocator, Backend, Box2D, CommittedFields, Display, FBox, ModeType, OwnedBuffer, Region,
    Renderer, Runtime, Transform, Until,
};

#[derive(Default)]
struct App {
    runtime: Option<Runtime>,
    staged_fields: Option<CommittedFields>,
    mode_none: Option<bool>,
    mode_custom: Option<bool>,
    staged_all: Option<CommittedFields>,
    commit_ok: Option<bool>,
    commit_after_abandon_ok: Option<bool>,
    headless: Option<bool>,
    // Written and read only where the backend predicates exist (see the
    // cfg gates below); allow dead otherwise rather than cfg'ing the shape.
    #[cfg_attr(not(all(wlr_has_drm_backend, wlr_has_x11_backend)), allow(dead_code))]
    backends: Option<(bool, bool, bool)>,
    adaptive_some: Option<bool>,
    name_round_trip: Option<bool>,
    name_nul_err: Option<bool>,
    desc_round_trip: Option<bool>,
    desc_nul_err: Option<bool>,
    try_null_misses: Option<bool>,
    effective: Option<(i32, i32)>,
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        // mode_type with no mode staged must be None, not a fabricated
        // Fixed: the raw field reads 0 on a fresh transaction.
        self.mode_none = Some(output.state().mode_type().is_none());

        // Stage everything committable headless in one atomic transaction.
        let mut st = output.state();
        st.set_enabled(true);
        st.set_custom_mode(1920, 1080, 60_000);
        st.set_scale(2.0);
        st.set_transform(Transform::Normal);
        st.set_adaptive_sync_enabled(false);
        st.set_render_format(0x3432_5258); // DRM_FORMAT_XRGB8888
        st.set_subpixel(wlr::Subpixel::None);
        st.set_damage(&Region::new());
        self.staged_fields = Some(st.committed_fields());
        self.mode_custom = Some(st.mode_type() == Some(ModeType::Custom));
        self.commit_ok = Some(st.commit().is_ok());

        // A dropped transaction finishes without committing: a fresh commit
        // right after must still succeed, proving the abandon corrupts
        // nothing.
        {
            let mut abandoned = output.state();
            abandoned.set_scale(3.0);
        }
        let again = output.state();
        self.commit_after_abandon_ok = Some(again.commit().is_ok());

        // Staging set_buffer needs a live `Buffer`, which no public API
        // hands out (only `BufferId`); its one-line FFI shape matches
        // `set_damage` above, which this mask pins. Tracked gap, not silent.
        self.staged_all = self.staged_fields;

        self.headless = Some(output.is_headless());
        // Backend predicates exist only with their backend features (see the
        // methods' cfg gates); headless + Wayland glue are always bound.
        #[cfg(all(wlr_has_drm_backend, wlr_has_x11_backend))]
        {
            self.backends = Some((output.is_drm(), output.is_wl(), output.is_x11()));
        }
        self.adaptive_some = Some(output.adaptive_sync_status().is_some());
        self.name_round_trip = Some(
            output.set_name("m6-output-test").is_ok()
                && output.name().as_deref() == Some("m6-output-test"),
        );
        self.name_nul_err = Some(output.set_name("a\0b").is_err());
        self.desc_round_trip = Some(
            output.set_description("m6 desc").is_ok()
                && output.description().as_deref() == Some("m6 desc"),
        );
        self.desc_nul_err = Some(output.set_description("d\0e").is_err());
        self.effective = Some(output.effective_resolution());

        // try_output_from_resource(null) must miss without touching C.
        // The non-null path needs a live wl_resource from a protocol
        // client, so it is e2e-only (icedtea harness).
        if let Some(rt) = &self.runtime {
            // SAFETY: null is the documented miss input; nothing is read.
            let miss = unsafe { rt.try_output_from_resource(std::ptr::null_mut()) };
            self.try_null_misses = Some(miss.is_none());
        }

        // Frame/scanout calls: smoke only (no trap, no abort). Their
        // client-observable effects need frame-event listener infrastructure
        // (still waived, with the M6-feedback slice), so no oracle exists
        // in-crate to assert against.
        output.update_needs_frame();
        let _ = output.needs_frame();
        output.send_frame();
        output.schedule_done();
        output.send_present(&wlr::PresentEvent {
            commit_seq: 0,
            presented: true,
            when: std::time::Duration::new(1, 0),
            seq: 0,
            refresh: 60_000,
            flags: wlr::PresentFlags::VSYNC,
        });
        output.lock_attach_render(true);
        output.lock_attach_render(false);
        output.lock_software_cursors(true);
        output.lock_software_cursors(false);
    }
}

impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}
impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        true
    }
}

/// Atomic state stages fields and commits them together on a headless
/// output; the accessors read back live truth.
#[test]
fn output_state_stages_fields_and_commits_atomically() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    let mut app = App {
        runtime: Some(runtime.clone()),
        ..App::default()
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");

    assert_eq!(
        app.mode_none,
        Some(true),
        "mode_type with no mode staged must be None, not fabricated Fixed"
    );
    assert_eq!(
        app.staged_fields,
        Some(
            CommittedFields::ENABLED
                | CommittedFields::SCALE
                | CommittedFields::ADAPTIVE_SYNC_ENABLED
                | CommittedFields::MODE
                | CommittedFields::TRANSFORM
                | CommittedFields::RENDER_FORMAT
                | CommittedFields::SUBPIXEL
                | CommittedFields::DAMAGE
        ),
        "the staged mask must name exactly the fields set"
    );
    assert_eq!(
        app.mode_custom,
        Some(true),
        "set_custom_mode must read back as Custom"
    );
    assert_eq!(
        app.commit_ok,
        Some(true),
        "the staged transaction must commit on headless"
    );
    assert_eq!(
        app.commit_after_abandon_ok,
        Some(true),
        "committing right after an abandoned transaction must still succeed"
    );
    assert_eq!(app.headless, Some(true), "headless backend headless");
    // Backend predicates exist only with their features (see the methods'
    // cfg gates); without drm/x11 backends there is nothing to assert.
    #[cfg(all(wlr_has_drm_backend, wlr_has_x11_backend))]
    assert_eq!(
        app.backends,
        Some((false, false, false)),
        "headless is none of drm/wl/x11"
    );
    assert_eq!(
        app.adaptive_some,
        Some(true),
        "adaptive-sync status must read back"
    );
    assert_eq!(
        app.name_round_trip,
        Some(true),
        "set_name must read back through name()"
    );
    assert_eq!(
        app.name_nul_err,
        Some(true),
        "interior NUL names must be rejected, not truncated"
    );
    assert_eq!(
        app.desc_round_trip,
        Some(true),
        "set_description must read back through description()"
    );
    assert_eq!(
        app.desc_nul_err,
        Some(true),
        "interior NUL descriptions must be rejected"
    );
    assert_eq!(
        app.try_null_misses,
        Some(true),
        "try_output_from_resource(null) must miss"
    );
    let (w, h) = app.effective.expect("effective resolution read");
    assert!(w > 0 && h > 0, "effective resolution must be positive");
}

/// `OutputState::copy_from` carries staged fields across transactions and
/// reports whether the copy ran.
#[test]
fn output_state_copy_carries_staged_fields() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    struct CopyApp {
        copied_fields: Option<CommittedFields>,
        copied_ok: Option<bool>,
        empty_fields: Option<CommittedFields>,
    }
    impl wlr::OutputHandler for CopyApp {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            let mut src = output.state();
            src.set_scale(2.0);
            let mut dst = output.state();
            let ok = dst.copy_from(&src);
            self.copied_ok = Some(ok);
            self.copied_fields = Some(dst.committed_fields());

            let mut empty_dst = output.state();
            let empty_src = output.state();
            let empty_ok = empty_dst.copy_from(&empty_src);
            self.empty_fields = Some(if empty_ok {
                empty_dst.committed_fields()
            } else {
                CommittedFields::from_bits(u32::MAX)
            });
        }
    }
    impl wlr::ToplevelHandler for CopyApp {}
    impl wlr::SeatHandler for CopyApp {}
    impl wlr::FdHandler for CopyApp {}
    impl wlr::LoopHandler for CopyApp {
        fn should_stop(&mut self) -> bool {
            true
        }
    }

    let mut app = CopyApp {
        copied_fields: None,
        copied_ok: None,
        empty_fields: None,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    assert_eq!(app.copied_ok, Some(true), "copy must report success");
    assert_eq!(
        app.copied_fields,
        Some(CommittedFields::SCALE),
        "copy_from must carry the staged scale bit"
    );
    assert_eq!(
        app.empty_fields,
        Some(CommittedFields::NONE),
        "copying an empty transaction stages nothing"
    );
}

/// Hardware cursor create/move/set_buffer/destroy round-trips on a headless
/// output, and output layers stage through an atomic transaction. A destroyed
/// cursor cannot be used again (the handle is consumed), so the
/// destroy-twice path is a compile error, not a runtime case.
#[test]
fn output_cursor_and_layers_round_trip() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    // The cursor image, built exactly as `tests/render.rs`'s
    // `an_allocator_hands_out_buffers_a_pass_can_draw_into` does: a pixman
    // renderer (no GPU needed) plus `Allocator::autocreate` on this backend,
    // then an 8x8 linear ARGB8888 buffer. If `autocreate` fails headless this
    // is a STOP, not something to fake with another allocator: the cursor
    // needs a real wlroots buffer.
    let renderer = Renderer::pixman().expect("pixman renderer");
    let allocator = Allocator::autocreate(&backend, &renderer).expect("allocator");
    let cursor_buf = allocator.create_buffer(8, 8, &argb()).expect("buffer");

    /// What the cursor episode in `new_output` records, kept as one value so
    /// the handler can borrow `self.cursor_buf` for the whole episode and
    /// write `self` only after the borrowing cursor is destroyed.
    struct CursorProbe {
        fresh_enabled: bool,
        fresh_moved: bool,
        fresh_size_zero: bool,
        buffered_enabled: bool,
        buffered_size: (u32, u32),
    }

    struct CursorApp<'a> {
        cursor_buf: OwnedBuffer<'a>,
        runtime: Runtime,
        output_enabled: Option<bool>,
        output_init: Option<bool>,
        moved: Option<bool>,
        size_zero: Option<bool>,
        enabled_after_create: Option<bool>,
        cursor_enabled_after_buffer: Option<bool>,
        cursor_size_after_buffer: Option<(u32, u32)>,
        layers_bit: Option<CommittedFields>,
        layers_buffered_fields: Option<CommittedFields>,
        layers_buffered_commit_ok: Option<bool>,
        empty_layers_fields: Option<CommittedFields>,
        empty_layers_commit_ok: Option<bool>,
        two_layers_fields: Option<CommittedFields>,
        two_layers_commit_ok: Option<bool>,
    }
    impl wlr::OutputHandler for CursorApp<'_> {
        fn new_output(&mut self, output: &wlr::Output<'_>) {
            // No expect() here: a panic inside a handler aborts through C
            // rather than failing the test. Record and assert afterwards.
            //
            // Enable + init render first: `wlr_output_cursor_set_buffer`
            // asserts `output->renderer != NULL`, which only
            // `Runtime::init_output` (`wlr_output_init_render`) establishes —
            // and init_output wants an enabled output. Both report Results,
            // so their outcomes join the locals below, never a panic.
            let enabled_ok = output.enable_with_preferred_mode().is_ok();
            let init_ok = self.runtime.init_output(output).is_ok();
            //
            // The cursor episode borrows `self.cursor_buf`, so everything it
            // learns goes to locals first and reaches `self` only after the
            // cursor (which holds that borrow) is destroyed. The image borrow
            // opens before `create_cursor` so the `&'b Buffer<'b>` outlives
            // the cursor it is staged on — image older than cursor, as usual.
            let cursor_outcome: Option<CursorProbe> = {
                let buf: &wlr::Buffer = &self.cursor_buf;
                if let Some(mut cursor) = output.create_cursor() {
                    let fresh_enabled = cursor.is_enabled();
                    let fresh_moved = cursor.move_to(10.0, 20.0);
                    // No buffer staged yet, so no size: a fresh cursor reads
                    // (0, 0), gaining extents only once a buffer is set.
                    let fresh_size_zero = cursor.size() == (0, 0);
                    cursor.set_buffer(buf, 1, 2);
                    let buffered_enabled = cursor.is_enabled();
                    let buffered_size = cursor.size();
                    cursor.destroy();
                    Some(CursorProbe {
                        fresh_enabled,
                        fresh_moved,
                        fresh_size_zero,
                        buffered_enabled,
                        buffered_size,
                    })
                } else {
                    None
                }
            };
            match cursor_outcome {
                Some(probe) => {
                    self.output_enabled = Some(enabled_ok);
                    self.output_init = Some(init_ok);
                    self.enabled_after_create = Some(probe.fresh_enabled);
                    self.moved = Some(probe.fresh_moved);
                    self.size_zero = Some(probe.fresh_size_zero);
                    self.cursor_enabled_after_buffer = Some(probe.buffered_enabled);
                    self.cursor_size_after_buffer = Some(probe.buffered_size);
                }
                None => {
                    self.output_enabled = Some(enabled_ok);
                    self.output_init = Some(init_ok);
                    self.enabled_after_create = Some(true);
                    self.moved = Some(false);
                    self.size_zero = Some(false);
                    self.cursor_enabled_after_buffer = Some(false);
                    self.cursor_size_after_buffer = Some((0, 0));
                }
            }

            let Some(layer) = output.create_layer() else {
                self.layers_bit = Some(CommittedFields::NONE);
                self.layers_buffered_fields = Some(CommittedFields::NONE);
                self.layers_buffered_commit_ok = Some(false);
                self.empty_layers_fields = Some(CommittedFields::from_bits(u32::MAX));
                self.empty_layers_commit_ok = Some(false);
                self.two_layers_fields = Some(CommittedFields::NONE);
                self.two_layers_commit_ok = Some(false);
                return;
            };
            // Scoped so every borrow of `layer` ends before `destroy()`:
            // `set_layers` copies the array synchronously, so nothing
            // staged outlives this block.
            {
                let region = Region::new();
                let entry = wlr::LayerState {
                    layer: &layer,
                    buffer: None,
                    src: FBox {
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 100.0,
                    },
                    dst: Box2D {
                        x: 0,
                        y: 0,
                        width: 64,
                        height: 64,
                    },
                    damage: &region,
                    accepted: true,
                };
                let mut st = output.state();
                st.set_layers(std::slice::from_ref(&entry));
                self.layers_bit = Some(st.committed_fields());
            }
            layer.destroy();

            // A buffer-bearing entry, committed: commit success is the
            // well-formedness oracle — it proves the staged array marshalled
            // into something wlroots accepts. Its limit is acknowledged: a
            // src/dst transpose would still commit, so transposition risk
            // stays covered e2e, not by this bit.
            let buffered: Option<(CommittedFields, bool)> = {
                let buf: &wlr::Buffer = &self.cursor_buf;
                if let Some(layer) = output.create_layer() {
                    let region = Region::new();
                    let entry = wlr::LayerState {
                        layer: &layer,
                        buffer: Some(buf),
                        src: FBox {
                            x: 0.0,
                            y: 0.0,
                            width: 8.0,
                            height: 8.0,
                        },
                        dst: Box2D {
                            x: 0,
                            y: 0,
                            width: 8,
                            height: 8,
                        },
                        damage: &region,
                        accepted: true,
                    };
                    let mut st = output.state();
                    st.set_layers(std::slice::from_ref(&entry));
                    let fields = st.committed_fields();
                    let ok = st.commit().is_ok();
                    layer.destroy();
                    Some((fields, ok))
                } else {
                    None
                }
            };
            match buffered {
                Some((fields, ok)) => {
                    self.layers_buffered_fields = Some(fields);
                    self.layers_buffered_commit_ok = Some(ok);
                }
                None => {
                    self.layers_buffered_fields = Some(CommittedFields::NONE);
                    self.layers_buffered_commit_ok = Some(false);
                }
            }

            // An empty slice still stages the LAYERS bit: an explicit "no
            // layers" is still a layers statement, and LAYERS is in
            // `WLR_OUTPUT_STATE_BACKEND_OPTIONAL`
            // (`wlr/interfaces/wlr_output.h`), which backends may ignore —
            // so the commit succeeds headless.
            let (empty_fields, empty_ok) = {
                let mut st = output.state();
                st.set_layers(&[]);
                (st.committed_fields(), st.commit().is_ok())
            };
            self.empty_layers_fields = Some(empty_fields);
            self.empty_layers_commit_ok = Some(empty_ok);

            // Two entries from two handles: one LAYERS bit, one clean commit.
            let two: Option<(CommittedFields, bool)> = {
                if let (Some(first), Some(second)) = (output.create_layer(), output.create_layer())
                {
                    let first_region = Region::new();
                    let second_region = Region::new();
                    let first_entry = wlr::LayerState {
                        layer: &first,
                        buffer: None,
                        src: FBox {
                            x: 0.0,
                            y: 0.0,
                            width: 100.0,
                            height: 100.0,
                        },
                        dst: Box2D {
                            x: 0,
                            y: 0,
                            width: 64,
                            height: 64,
                        },
                        damage: &first_region,
                        accepted: true,
                    };
                    let second_entry = wlr::LayerState {
                        layer: &second,
                        buffer: None,
                        src: FBox {
                            x: 0.0,
                            y: 0.0,
                            width: 50.0,
                            height: 50.0,
                        },
                        dst: Box2D {
                            x: 64,
                            y: 0,
                            width: 32,
                            height: 32,
                        },
                        damage: &second_region,
                        accepted: false,
                    };
                    let entries = [first_entry, second_entry];
                    let mut st = output.state();
                    st.set_layers(&entries);
                    let fields = st.committed_fields();
                    let ok = st.commit().is_ok();
                    first.destroy();
                    second.destroy();
                    Some((fields, ok))
                } else {
                    None
                }
            };
            match two {
                Some((fields, ok)) => {
                    self.two_layers_fields = Some(fields);
                    self.two_layers_commit_ok = Some(ok);
                }
                None => {
                    self.two_layers_fields = Some(CommittedFields::NONE);
                    self.two_layers_commit_ok = Some(false);
                }
            }
        }
    }
    impl wlr::ToplevelHandler for CursorApp<'_> {}
    impl wlr::SeatHandler for CursorApp<'_> {}
    impl wlr::FdHandler for CursorApp<'_> {}
    impl wlr::LoopHandler for CursorApp<'_> {
        fn should_stop(&mut self) -> bool {
            true
        }
    }

    let mut app = CursorApp {
        cursor_buf,
        runtime: runtime.clone(),
        output_enabled: None,
        output_init: None,
        moved: None,
        size_zero: None,
        enabled_after_create: None,
        cursor_enabled_after_buffer: None,
        cursor_size_after_buffer: None,
        layers_bit: None,
        layers_buffered_fields: None,
        layers_buffered_commit_ok: None,
        empty_layers_fields: None,
        empty_layers_commit_ok: None,
        two_layers_fields: None,
        two_layers_commit_ok: None,
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    // The output must be enabled and render-initialised before any cursor
    // buffer work: `init_output` is what gives the output the renderer
    // `wlr_output_cursor_set_buffer` asserts on.
    assert_eq!(
        app.output_enabled,
        Some(true),
        "headless output must enable at its preferred mode"
    );
    assert_eq!(
        app.output_init,
        Some(true),
        "init_output must give the output a renderer"
    );
    // `wlr_output_cursor_move` on a fresh headless output: headless
    // `wlr_output_impl` provides no `move_cursor`, so the cursor never
    // becomes the hardware cursor and the move takes the software-cursor
    // path in `types/output/cursor.c`, which returns true (same-position
    // early-true, still-hidden early-true, damage-and-true). False comes
    // only from a backend `move_cursor` impl returning false, which no
    // headless run reaches — so assert the reachable true and note the
    // false branch stays untracked rather than forcing a false that is
    // not real.
    assert_eq!(
        app.moved,
        Some(true),
        "cursor move must be accepted headless"
    );
    assert_eq!(
        app.size_zero,
        Some(true),
        "fresh cursor with no buffer must report (0, 0)"
    );
    assert_eq!(
        app.enabled_after_create,
        Some(false),
        "cursor with no buffer is not enabled"
    );
    assert_eq!(
        app.cursor_enabled_after_buffer,
        Some(true),
        "set_buffer must enable the cursor"
    );
    assert_eq!(
        app.cursor_size_after_buffer,
        Some((8, 8)),
        "cursor image size must match the staged buffer"
    );
    assert_eq!(
        app.layers_bit,
        Some(CommittedFields::LAYERS),
        "set_layers must stage exactly the layers bit"
    );
    assert_eq!(
        app.layers_buffered_fields,
        Some(CommittedFields::LAYERS),
        "a buffer-bearing entry must stage exactly the layers bit"
    );
    assert_eq!(
        app.layers_buffered_commit_ok,
        Some(true),
        "buffer-bearing layers must commit headless (well-formedness oracle)"
    );
    assert_eq!(
        app.empty_layers_fields,
        Some(CommittedFields::LAYERS),
        "an empty set_layers still stages the layers bit"
    );
    assert_eq!(
        app.empty_layers_commit_ok,
        Some(true),
        "empty layers must commit headless"
    );
    assert_eq!(
        app.two_layers_fields,
        Some(CommittedFields::LAYERS),
        "two entries must stage exactly the layers bit"
    );
    assert_eq!(
        app.two_layers_commit_ok,
        Some(true),
        "two entries must commit headless"
    );
}
