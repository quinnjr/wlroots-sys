#![no_main]
//! Cumulative operation-sequence fuzz target for `wlr`'s stateful wrappers.
//!
//! libFuzzer replays an arbitrary `Vec<Operation>` against a headless
//! compositor brought up once per process; AddressSanitizer (which `cargo-fuzz`
//! links for this target) is the use-after-free oracle. The operations are
//! compositor-side calls into the safe `wlr` API only — no raw pointers, no
//! `sys` module.
//!
//! # Why the compositor is process-global, not per input
//!
//! `Runtime::init_graphics` hands its scene, output layout, renderer and
//! allocator to wlroots and never frees them (`Graphics` has no `Drop`;
//! `runtime.rs`'s own comment says "nothing created here is ever freed by this
//! crate at all"). A compositor owns them for its whole life, so that is the
//! correct release shape — but it means a *fresh* runtime per fuzz input leaks
//! a scene per input and would exhaust memory long before a scheduled run ends.
//! One compositor for the process keeps that allocation one-time and bounded.
//! Two consequences follow, and both are accepted for this seed:
//!
//! * **Inputs are not independent.** Some state persists between inputs, so a
//!   crash is not guaranteed to reproduce from its recorded artifact alone:
//!   `cargo fuzz run operations <artifact>` replays that input against a
//!   *fresh* compositor, not the accumulated one. The reachable operation set
//!   is chosen to be miss-safe and idempotent, so the dependence is weak — but
//!   it is real, and the artifact is a starting point, not a complete repro.
//! * **The create guards saturate.** Once any input creates a manager global,
//!   every later `Create*` op returns `Err`; only the double-create refusal
//!   stays live, and the successful-create path is reached at most once per
//!   process.
//!
//! # Why the operations drive from Rust, never from a handler
//!
//! wlroots emits its signals synchronously, from inside its own API calls, so a
//! handler always runs underneath an `extern "C"` frame: a panic escaping one
//! aborts the process instead of unwinding. Every operation here is therefore
//! issued from the replay loop in plain Rust, and the one handler this target
//! installs ([`Recorder`]) only stores an id — it never asserts, indexes,
//! unwraps, or calls back into `wlr`.
//!
//! # What the target can and cannot reach today
//!
//! A real `Toplevel`, `Popup` or `LayerSurface` is created by a wayland client
//! connecting to a running compositor; the fuzz crate deliberately carries no
//! client dependency, so no operation here can mint a *live* one. The by-id
//! mutators are instead driven with ids from the reserved "dangling" band the
//! wrappers expose for exactly this (`ToplevelId::dangling_nth_for_test`,
//! `PopupId::dangling_nth_for_test`, `LayerSurfaceId::dangling_for_test`).
//! That is not a no-op: it exercises the id-table lookup, the liveness check
//! and the frozen "an unknown id is a miss, never a dereference" contract,
//! which is the memory-safety boundary a client-driven path would eventually
//! cross. Operations that genuinely need a client (creating a toplevel or
//! popup, entering a session lock, redeeming an activation token) are omitted
//! rather than stubbed. What remains reachable without a client is exactly the
//! manager/global double-create guards, the shared id-resolution/miss contract,
//! and the client-free state queries. Client-driven create/commit/ack/destroy
//! fuzzing is **deferred until the fuzz crate takes a `wayland-client`
//! dependency**; when that lands, each state machine appends its operations
//! here, and the enum is cumulative.
//!
//! [`Recorder`] captures the headless output's `OutputId` from one short
//! `Backend::run_all` so the output- and layer-config operations have a real id
//! to hand the wrappers. The run has returned by the time they execute, so the
//! id is stale by design: those operations prove the documented
//! stale-id-misses-cleanly boundary rather than configuring a live output.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::cell::OnceCell;

/// One compositor-side operation on a stateful wrapper.
///
/// # Scope: reachable-without-a-client only
///
/// This is the brief's `Operation` seed. The brief named `CreateToplevel`,
/// `CreatePopup`, `ConfigureToplevel` and `DestroyToplevel`. The *live-object*
/// forms of all four need a connected wayland client and are rejected here: a
/// real `Toplevel`/`Popup` exists only while a client is connected, and the
/// fuzz crate deliberately carries no `wayland-client` dependency. Where a
/// client-free *dangling-id miss* form exists it is kept rather than omitted —
/// [`ConfigureToplevel`] is the by-id `Runtime::configure_toplevel` call on an
/// id that can only miss (see the by-id contract below), not a live
/// reconfiguration of a client's toplevel. Per the task rule ("a variant whose
/// API you cannot drive yet should be omitted rather than left as a no-op
/// stub"), the committed set otherwise covers what *is* reachable from Rust:
///
/// * **manager/global double-create guards** — `CreateXdgShell`,
///   `CreateLayerShell`, `CreateActivationManager`,
///   `CreateSessionLockManager`, `CreateTextInputManager`,
///   `CreateInputMethodManager`, `CreateOutputManager`;
/// * **the shared by-id lookup/miss contract** — every `SetToplevel*`,
///   `Popup*`, layer and output operation resolves a `*Id` through the same
///   id-table path and must miss cleanly on an unknown one;
/// * **state queries** — `QuerySessionLocked`, `InputMethodActive`, the IME
///   snapshots, `ScheduleFrameAll`.
///
/// Client-driven create/commit/ack/destroy fuzzing — the operation sequences
/// the roadmap ultimately wants — is **deferred until the fuzz crate takes a
/// `wayland-client` dependency** and can bind real protocol objects. When that
/// lands, append the create/destroy variants here; the enum is cumulative.
///
/// Field names name the argument, not the C call: `nth` selects a reserved
/// dangling id for the by-id mutators, and carries no meaning beyond giving a
/// sequence several distinct unknown ids.
#[derive(Arbitrary, Debug)]
enum Operation {
    // --- xdg-shell global (double-create guard only; the toplevel/popup
    // state machine itself needs a connected client) ---
    /// `Runtime::create_xdg_shell`; repeated ops exercise only the
    /// double-create guard, since no client can mint a toplevel here.
    CreateXdgShell,

    // --- Toplevel: `runtime.rs` by-id mutators, ids from `toplevel.rs` ---
    /// `Runtime::set_toplevel_size`.
    SetToplevelSize { nth: u64, width: i32, height: i32 },
    /// `Runtime::set_toplevel_activated`.
    SetToplevelActivated { nth: u64, activated: bool },
    /// `Runtime::set_toplevel_maximized`.
    SetToplevelMaximized { nth: u64, maximized: bool },
    /// `Runtime::set_toplevel_fullscreen`.
    SetToplevelFullscreen { nth: u64, fullscreen: bool },
    /// `Runtime::set_toplevel_position`.
    SetToplevelPosition { nth: u64, x: i32, y: i32 },
    /// `Runtime::set_toplevel_visible`.
    SetToplevelVisible { nth: u64, visible: bool },
    /// `Runtime::raise_toplevel`.
    RaiseToplevel { nth: u64 },
    /// `Runtime::configure_toplevel`.
    ConfigureToplevel { nth: u64 },
    /// `Runtime::close_toplevel`.
    CloseToplevel { nth: u64 },
    /// `Runtime::set_toplevel_bounds`.
    SetToplevelBounds { nth: u64, width: i32, height: i32 },
    /// `Runtime::set_toplevel_constrained`.
    SetToplevelConstrained {
        nth: u64,
        top: bool,
        bottom: bool,
        left: bool,
        right: bool,
    },
    /// `Runtime::set_toplevel_parent`; both ids are dangling, so the parent
    /// lookup misses before the assignment is attempted.
    SetToplevelParent { nth: u64, parent_nth: u64 },
    /// `Runtime::set_toplevel_resizing`.
    SetToplevelResizing { nth: u64, resizing: bool },
    /// `Runtime::set_toplevel_suspended`.
    SetToplevelSuspended { nth: u64, suspended: bool },
    /// `Runtime::set_toplevel_tiled`.
    SetToplevelTiled {
        nth: u64,
        top: bool,
        bottom: bool,
        left: bool,
        right: bool,
    },
    /// `Runtime::set_toplevel_wm_capabilities`.
    SetToplevelWmCapabilities { nth: u64, caps: u32 },
    /// `Runtime::decoration_state`.
    DecorationState { nth: u64 },
    /// `Runtime::decoration_configure`.
    DecorationConfigure { nth: u64 },
    /// `Runtime::set_decoration_mode`.
    SetDecorationMode { nth: u64, server_side: bool },
    /// `Runtime::focus_toplevel_keyboard`.
    FocusToplevelKeyboard { nth: u64 },
    /// `Runtime::toplevel_at` — the hit-test entry point.
    ToplevelAt { x: f64, y: f64 },

    // --- Popup: `runtime.rs` by-id reads and mutators, ids from `popup.rs` ---
    /// `Runtime::popup_parent`.
    PopupParentOf { nth: u64 },
    /// `Runtime::popups_of`.
    PopupsOf { nth: u64 },
    /// `Runtime::popup_chain`.
    PopupChain { nth: u64 },
    /// `Runtime::configure_popup`.
    ConfigurePopup {
        nth: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// `Runtime::popup_position`.
    PopupPosition { nth: u64 },
    /// `Runtime::dismiss_popup`.
    DismissPopup { nth: u64 },
    /// `Runtime::dismiss_popups_of`.
    DismissPopupsOf { nth: u64 },
    /// `Runtime::popup_is_grabbing`.
    PopupIsGrabbing { nth: u64 },

    // --- Layer: `runtime.rs` by-id ops, ids from `layer.rs` ---
    /// `Runtime::create_layer_shell`; double-create guard included.
    CreateLayerShell,
    /// `Runtime::configure_layer_surface`.
    ConfigureLayerSurface { width: u32, height: u32 },
    /// `Runtime::set_layer_surface_position`.
    SetLayerSurfacePosition { x: i32, y: i32 },
    /// `Runtime::focus_layer_keyboard`.
    FocusLayerKeyboard,
    /// `Runtime::set_layer_surface_output` against the captured (stale) output
    /// id. The layer id is resolved first, so this stays on the miss path.
    SetLayerSurfaceOutput,

    // --- Activation: `runtime.rs` ---
    /// `Runtime::create_xdg_activation_manager`; double-create guard included.
    CreateActivationManager,

    // --- Session lock: `runtime.rs` ---
    /// `Runtime::create_session_lock_manager`; double-create guard included.
    CreateSessionLockManager,
    /// `Runtime::is_session_locked`.
    QuerySessionLocked,

    // --- Input method / IME: `runtime.rs` ---
    /// `Runtime::create_text_input_manager`; double-create guard included.
    CreateTextInputManager,
    /// `Runtime::create_input_method_manager`; double-create guard included.
    CreateInputMethodManager,
    /// `Runtime::input_method_active`.
    InputMethodActive,
    /// `Runtime::pending_ime_state`.
    PendingImeState,
    /// `Runtime::committed_ime_state`.
    CommittedImeState,

    // --- Output configuration: `runtime.rs` ---
    /// `Runtime::create_output_manager`; double-create guard included.
    CreateOutputManager,
    /// `Runtime::update_output_manager_state`.
    UpdateOutputManagerState,
    /// `Runtime::schedule_frame_all`.
    ScheduleFrameAll,
    /// `Runtime::output_layout_box` against the captured (stale) id.
    OutputLayoutBox,
    /// `Runtime::set_output_position` against the captured (stale) id.
    SetOutputPosition { x: i32, y: i32 },
    /// `Runtime::schedule_frame` against the captured (stale) id.
    ScheduleFrame,

    // --- Presentation feedback + tearing (M9): `presentation.rs`/`tearing.rs`
    // by-id reads, ids from `surface.rs` ---
    /// `Runtime::create_tearing_control_manager`; double-create guard included.
    CreateTearingControlManager,
    /// `Runtime::sample_presentation` against a dangling surface id. A live
    /// surface needs a connected client, so this stays on the miss path.
    SamplePresentation { nth: u64 },
    /// `Runtime::tearing_hint` against a dangling surface id.
    TearingHint { nth: u64 },
    /// `Runtime::tearing_control` against a dangling surface id.
    TearingControlOf { nth: u64 },

    // --- Sub-surface role (M9): `subsurface.rs`, reached by id from
    // `surface.rs`. A positive path needs a connected client to mint a real
    // sub-surface, so these stay on the by-id miss the surface lookup reports.
    /// `Surface::subsurface_parent_id` against a dangling surface id.
    SubsurfaceParentId { nth: u64 },
    /// `Surface::subsurface_parent_state` against a dangling surface id.
    SubsurfaceParentState { nth: u64 },
    /// `Runtime::toplevel_of` against a dangling surface id.
    ToplevelOf { nth: u64 },
    /// `Runtime::popup_of` against a dangling surface id.
    PopupOf { nth: u64 },
}

/// The one handler this target installs.
///
/// `Backend::run_all` calls it from underneath wlroots' `extern "C"` frames, so
/// every method here is deliberately inert: `new_output` stores the announced
/// id (a plain `Option` assignment) and nothing else does anything. No assert,
/// no unwrap, no index, no re-entry into `wlr`.
#[derive(Default)]
struct Recorder {
    output: Option<wlr::OutputId>,
    turns: u32,
}

impl wlr::OutputHandler for Recorder {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        if self.output.is_none() {
            self.output = Some(output.id());
        }
    }
}

impl wlr::ToplevelHandler for Recorder {}
impl wlr::SeatHandler for Recorder {}
impl wlr::FdHandler for Recorder {}

impl wlr::LoopHandler for Recorder {
    fn should_stop(&mut self) -> bool {
        self.turns += 1;
        self.turns >= 2
    }
}

/// The one headless compositor this process builds.
///
/// Bring-up failure is fatal: each failing step panics with the underlying
/// error rather than caching a silent `None`, so a broken headless/pixman/
/// wlroots path aborts the run instead of producing a green job that fuzzed
/// nothing. The `Option` return remains only because the thread-local cell
/// stores it; it is always `Some` on return.
///
/// A `&'static` in a thread-local cell rather than an owned value: `Backend`
/// borrows the `EventLoop` it was created from, and `Runtime` must not outlive
/// the `Display` — a self-referential trio that is simplest to `Box::leak` once
/// and never drop. libFuzzer drives this target on a single thread, so the
/// thread-local is the right scope, and `Option<&'static _>` is `Copy`, which
/// lets `with` hand the reference out without borrowing the cell.
struct Compositor {
    /// Dropped before `_backend` and `_display`, mirroring every integration
    /// test's declaration order (`init_graphics` requires the runtime not to
    /// outlive the display).
    runtime: wlr::Runtime,
    /// The headless output announced by the one setup run, kept so the
    /// output-config operations have a real (stale) id to try.
    output: Option<wlr::OutputId>,
    _backend: wlr::Backend<'static>,
    _display: &'static wlr::Display,
}

thread_local! {
    static COMPOSITOR: OnceCell<Option<&'static Compositor>> = const { OnceCell::new() };
}

/// Build the compositor on first use, then return it.
///
/// `WLR_BACKENDS` and friends are set before `Backend::autocreate` reads them.
/// Any bring-up failure panics rather than returning `None`, so the fuzzer
/// cannot mistake an unstartable harness for a clean input.
fn compositor() -> Option<&'static Compositor> {
    COMPOSITOR.with(|cell| {
        *cell.get_or_init(|| {
            // `fuzz/Cargo.toml` pins this crate to edition 2021, where
            // `set_var` is a safe call.
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");

            let display: &'static wlr::Display = Box::leak(Box::new(
                wlr::Display::new()
                    .unwrap_or_else(|e| panic!("fuzz harness could not start: {e}")),
            ));
            let event_loop: &'static wlr::EventLoop<'static> =
                Box::leak(Box::new(display.event_loop()));
            let backend = wlr::Backend::autocreate(event_loop)
                .unwrap_or_else(|e| panic!("fuzz harness could not start: {e}"));
            let runtime = wlr::Runtime::new()
                .unwrap_or_else(|e| panic!("fuzz harness could not start: {e}"));
            runtime
                .init_graphics(display, &backend)
                .unwrap_or_else(|e| panic!("fuzz harness could not start: {e}"));

            // One short run so the headless backend announces its output.
            // `Recorder::new_output` only stores the id. When the run returns
            // the id is stale by design (output tables are per-run), so the
            // output-config operations exercise the documented
            // stale-id-misses-cleanly boundary rather than a live output.
            let mut recorder = Recorder::default();
            match backend.run_all(display, &mut recorder, &runtime, wlr::Until::Turns(4)) {
                Ok(()) => {}
                Err(e) => panic!("fuzz setup run failed: {e}"),
            }
            if recorder.output.is_none() {
                panic!("fuzz harness could not start: setup run produced no output");
            }

            Some(Box::leak(Box::new(Compositor {
                runtime,
                output: recorder.output,
                _backend: backend,
                _display: display,
            })))
        })
    })
}

fuzz_target!(|ops: Vec<Operation>| {
    let compositor = compositor()
        .expect("fuzz harness could not start: compositor setup returned None");
    for op in &ops {
        apply(
            &compositor.runtime,
            compositor._display,
            compositor.output,
            op,
        );
    }
});

/// Drive one operation. Every wrapper call's result is discarded — the fuzz
/// target asserts nothing at runtime; its only oracle is that the process
/// survives and ASan sees no invalid access.
fn apply(
    runtime: &wlr::Runtime,
    display: &wlr::Display,
    output: Option<wlr::OutputId>,
    op: &Operation,
) {
    use wlr::{
        Box2D, DecorationMode, LayerSurfaceId, PopupId, PopupParent, SurfaceId, ToplevelId,
        WmCapabilities,
    };

    let toplevel = |nth: u64| ToplevelId::dangling_nth_for_test(nth);
    let popup = |nth: u64| PopupId::dangling_nth_for_test(nth);

    match op {
        Operation::CreateXdgShell => {
            let _ = runtime.create_xdg_shell(display, 6);
        }

        Operation::SetToplevelSize { nth, width, height } => {
            let _ = runtime.set_toplevel_size(toplevel(*nth), *width, *height);
        }
        Operation::SetToplevelActivated { nth, activated } => {
            let _ = runtime.set_toplevel_activated(toplevel(*nth), *activated);
        }
        Operation::SetToplevelMaximized { nth, maximized } => {
            let _ = runtime.set_toplevel_maximized(toplevel(*nth), *maximized);
        }
        Operation::SetToplevelFullscreen { nth, fullscreen } => {
            let _ = runtime.set_toplevel_fullscreen(toplevel(*nth), *fullscreen);
        }
        Operation::SetToplevelPosition { nth, x, y } => {
            let _ = runtime.set_toplevel_position(toplevel(*nth), *x, *y);
        }
        Operation::SetToplevelVisible { nth, visible } => {
            let _ = runtime.set_toplevel_visible(toplevel(*nth), *visible);
        }
        Operation::RaiseToplevel { nth } => {
            let _ = runtime.raise_toplevel(toplevel(*nth));
        }
        Operation::ConfigureToplevel { nth } => {
            let _ = runtime.configure_toplevel(toplevel(*nth));
        }
        Operation::CloseToplevel { nth } => {
            let _ = runtime.close_toplevel(toplevel(*nth));
        }
        Operation::SetToplevelBounds { nth, width, height } => {
            let _ = runtime.set_toplevel_bounds(toplevel(*nth), *width, *height);
        }
        Operation::SetToplevelConstrained {
            nth,
            top,
            bottom,
            left,
            right,
        } => {
            let _ = runtime.set_toplevel_constrained(
                toplevel(*nth),
                wlr::Edges {
                    top: *top,
                    bottom: *bottom,
                    left: *left,
                    right: *right,
                },
            );
        }
        Operation::SetToplevelParent { nth, parent_nth } => {
            let _ = runtime.set_toplevel_parent(toplevel(*nth), Some(toplevel(*parent_nth)));
        }
        Operation::SetToplevelResizing { nth, resizing } => {
            let _ = runtime.set_toplevel_resizing(toplevel(*nth), *resizing);
        }
        Operation::SetToplevelSuspended { nth, suspended } => {
            let _ = runtime.set_toplevel_suspended(toplevel(*nth), *suspended);
        }
        Operation::SetToplevelTiled {
            nth,
            top,
            bottom,
            left,
            right,
        } => {
            let _ = runtime.set_toplevel_tiled(
                toplevel(*nth),
                wlr::Edges {
                    top: *top,
                    bottom: *bottom,
                    left: *left,
                    right: *right,
                },
            );
        }
        Operation::SetToplevelWmCapabilities { nth, caps } => {
            let mut c = WmCapabilities::NONE;
            if caps & 1 != 0 {
                c |= WmCapabilities::WINDOW_MENU;
            }
            if caps & 2 != 0 {
                c |= WmCapabilities::MAXIMIZE;
            }
            if caps & 4 != 0 {
                c |= WmCapabilities::FULLSCREEN;
            }
            if caps & 8 != 0 {
                c |= WmCapabilities::MINIMIZE;
            }
            let _ = runtime.set_toplevel_wm_capabilities(toplevel(*nth), c);
        }
        Operation::DecorationState { nth } => {
            let _ = runtime.decoration_state(toplevel(*nth));
        }
        Operation::DecorationConfigure { nth } => {
            let _ = runtime.decoration_configure(toplevel(*nth));
        }
        Operation::SetDecorationMode { nth, server_side } => {
            let mode = if *server_side {
                DecorationMode::ServerSide
            } else {
                DecorationMode::ClientSide
            };
            let _ = runtime.set_decoration_mode(toplevel(*nth), mode);
        }
        Operation::FocusToplevelKeyboard { nth } => {
            let _ = runtime.focus_toplevel_keyboard(toplevel(*nth));
        }
        Operation::ToplevelAt { x, y } => {
            let _ = runtime.toplevel_at(*x, *y);
        }

        Operation::PopupParentOf { nth } => {
            let _ = runtime.popup_parent(popup(*nth));
        }
        Operation::PopupsOf { nth } => {
            let _ = runtime.popups_of(PopupParent::Popup(popup(*nth)));
        }
        Operation::PopupChain { nth } => {
            let _ = runtime.popup_chain(PopupParent::Popup(popup(*nth)));
        }
        Operation::ConfigurePopup {
            nth,
            x,
            y,
            width,
            height,
        } => {
            let _ = runtime.configure_popup(popup(*nth), &Box2D::new(*x, *y, *width, *height));
        }
        Operation::PopupPosition { nth } => {
            let _ = runtime.popup_position(popup(*nth));
        }
        Operation::DismissPopup { nth } => {
            let _ = runtime.dismiss_popup(popup(*nth));
        }
        Operation::DismissPopupsOf { nth } => {
            let _ = runtime.dismiss_popups_of(PopupParent::Popup(popup(*nth)));
        }
        Operation::PopupIsGrabbing { nth } => {
            let _ = runtime.popup_is_grabbing(popup(*nth));
        }

        Operation::CreateLayerShell => {
            let _ = runtime.create_layer_shell(display, 4);
        }
        Operation::ConfigureLayerSurface { width, height } => {
            let _ = runtime.configure_layer_surface(
                LayerSurfaceId::dangling_for_test(),
                *width,
                *height,
            );
        }
        Operation::SetLayerSurfacePosition { x, y } => {
            let _ = runtime.set_layer_surface_position(LayerSurfaceId::dangling_for_test(), *x, *y);
        }
        Operation::FocusLayerKeyboard => {
            let _ = runtime.focus_layer_keyboard(LayerSurfaceId::dangling_for_test());
        }
        Operation::SetLayerSurfaceOutput => {
            if let Some(output) = output {
                let _ =
                    runtime.set_layer_surface_output(LayerSurfaceId::dangling_for_test(), output);
            }
        }

        Operation::CreateActivationManager => {
            let _ = runtime.create_xdg_activation_manager(display);
        }

        Operation::CreateSessionLockManager => {
            let _ = runtime.create_session_lock_manager(display);
        }
        Operation::QuerySessionLocked => {
            let _ = runtime.is_session_locked();
        }

        Operation::CreateTextInputManager => {
            let _ = runtime.create_text_input_manager(display);
        }
        Operation::CreateInputMethodManager => {
            let _ = runtime.create_input_method_manager(display);
        }
        Operation::InputMethodActive => {
            let _ = runtime.input_method_active();
        }
        Operation::PendingImeState => {
            let _ = runtime.pending_ime_state();
        }
        Operation::CommittedImeState => {
            let _ = runtime.committed_ime_state();
        }

        Operation::CreateOutputManager => {
            let _ = runtime.create_output_manager(display);
        }
        Operation::UpdateOutputManagerState => {
            runtime.update_output_manager_state();
        }
        Operation::ScheduleFrameAll => {
            let _ = runtime.schedule_frame_all();
        }
        Operation::OutputLayoutBox => {
            if let Some(output) = output {
                let _ = runtime.output_layout_box(output);
            }
        }
        Operation::SetOutputPosition { x, y } => {
            if let Some(output) = output {
                let _ = runtime.set_output_position(output, *x, *y);
            }
        }
        Operation::ScheduleFrame => {
            if let Some(output) = output {
                let _ = runtime.schedule_frame(output);
            }
        }

        Operation::CreateTearingControlManager => {
            let _ = runtime.create_tearing_control_manager(display, 1);
        }
        Operation::SamplePresentation { nth } => {
            let _ = runtime.sample_presentation(SurfaceId::dangling_nth_for_test(*nth));
        }
        Operation::TearingHint { nth } => {
            let _ = runtime.tearing_hint(SurfaceId::dangling_nth_for_test(*nth));
        }
        Operation::TearingControlOf { nth } => {
            let _ = runtime.tearing_control(SurfaceId::dangling_nth_for_test(*nth));
        }

        Operation::SubsurfaceParentId { nth } => {
            let _ = runtime
                .surface(SurfaceId::dangling_nth_for_test(*nth))
                .map(|surface| surface.subsurface_parent_id());
        }
        Operation::SubsurfaceParentState { nth } => {
            let _ = runtime
                .surface(SurfaceId::dangling_nth_for_test(*nth))
                .map(|surface| surface.subsurface_parent_state());
        }
        Operation::ToplevelOf { nth } => {
            let _ = runtime.toplevel_of(SurfaceId::dangling_nth_for_test(*nth));
        }
        Operation::PopupOf { nth } => {
            let _ = runtime.popup_of(SurfaceId::dangling_nth_for_test(*nth));
        }
    }
}

/// LeakSanitizer suppressions for this target.
///
/// Leak detection stays on so a genuine leak this crate owns is still reported.
/// Only the known process-lifetime allocations are suppressed: wlroots and
/// libwayland keep process-global state (`wlr_`/`wl_` prefixes), and
/// `Runtime::init_graphics` deliberately never frees the scene, output layout,
/// renderer or allocator — a real compositor owns them until it exits, so they
/// outlive every fuzz input by design. Neither is a defect a run can act on.
/// The use-after-free/overflow oracle this target exists for is unaffected.
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn __lsan_default_suppressions() -> *const std::os::raw::c_char {
    b"leak:wlr_\nleak:wl_\0".as_ptr().cast()
}
