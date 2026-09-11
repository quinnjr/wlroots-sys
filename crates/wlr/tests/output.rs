//! Output atomic state and accessors, against a real headless backend.
//!
//! Same shape as the other per-binary `headless_env` helpers: this
//! integration binary owns its environment (`Display::new` +
//! `Backend::autocreate` + `Runtime::new` + `init_graphics`, keeping
//! `display` a live local). Handler observations are recorded on `App`
//! and asserted after the run — never inside a handler, where a panic
//! would abort through C.

use std::sync::Once;
use wlr::{Backend, CommittedFields, Display, ModeType, Region, Runtime, Transform, Until};

/// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once, before
/// any test in this binary calls `Backend::autocreate`. See `axis.rs`'s
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
