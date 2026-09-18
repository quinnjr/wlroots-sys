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
//!   Timeout-skips add a second, load-dependent nondeterminism of the same
//!   kind: a client op that connects on an idle machine may hit
//!   `CLIENT_IO_TIMEOUT` and skip on a loaded one, so two runs of the same
//!   corpus can drive different live-op sets.
//! * **The create guards saturate — accepted.** Once any input creates a
//!   manager global, every later `Create*` op returns `Err`; only the
//!   double-create refusal stays live, and the successful-create path runs at
//!   most once per process. The checked-in seeds (see below) order each
//!   `Create*` before its dependents so the success path is still reached
//!   deterministically; later inputs pin the refusal half of the contract.
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
//! Two reaches coexist. The by-id mutators are driven with ids from the
//! reserved "dangling" band the wrappers expose for exactly this
//! (`ToplevelId::dangling_nth_for_test`, `PopupId::dangling_nth_for_test`,
//! `LayerSurfaceId::dangling_for_test`). That is not a no-op: it exercises the
//! id-table lookup, the liveness check and the frozen "an unknown id is a
//! miss, never a dereference" contract, which is the memory-safety boundary a
//! client-driven path would eventually cross. Operations that genuinely need a
//! client beyond the toplevel lifecycle (creating a popup, entering a session
//! lock, redeeming an activation token) are omitted rather than stubbed. What
//! remains reachable without a client is exactly the manager/global
//! double-create guards, the shared id-resolution/miss contract, and the
//! client-free state queries.
//!
//! Since the fuzz crate took a `wayland-client` dependency, the client-driven
//! operations (`Operation::ClientToplevelLifecycle`,
//! `Operation::ClientToplevelBurst`) additionally mint *live* toplevels: a
//! synchronous same-thread client connects to the harness socket, creates a
//! `wl_surface` + `xdg_surface` + `xdg_toplevel`, commits, and the replay loop
//! interleaves client flushes with server `Backend::run_all` turns and client
//! `dispatch_pending` (which acks configures), then destroys/unmaps. That
//! reaches the live announcement/commit/configure/ack/destroy machinery with
//! real objects — the state-machine code the dangling-id ops cannot touch.
//! One honest boundary remains: a `ToplevelId` stops resolving once the
//! `run_all` that announced it returns (see `ToplevelId`'s own docs), so the
//! by-id calls the replay loop issues against the captured id *after* its
//! announcing pump observe the documented stale-miss boundary. The live
//! driving itself happens server-side, during the pumps, under ASan.
//!
//! [`Recorder`] captures the headless output's `OutputId` from one short
//! `Backend::run_all` so the output- and layer-config operations have a real id
//! to hand the wrappers. The run has returned by the time they execute, so the
//! id is stale by design: those operations prove the documented
//! stale-id-misses-cleanly boundary rather than configuring a live output.
//!
//! # What the integration tests own, and what this target owns
//!
//! The positive paths — live toplevels, popups, layers, surfaces, foreign
//! handles, workspaces, activation tokens, presentation feedback — belong to
//! the integration tests (`crates/wlr/tests/toplevels.rs`, `popups.rs`,
//! `layers.rs`, `surfaces.rs`, `foreign_toplevel.rs`, `ext_protocols.rs`,
//! `presentation.rs`, `xdg_protocols.rs`, `xdg_remainder.rs`), which are the
//! logic oracle for this target. What those tests cannot do is run under
//! AddressSanitizer against adversarial *sequences*: that split is explicit
//! and deliberate. This target pins the other half — the frozen
//! unknown-id-misses-cleanly and double-create-refused contracts
//! (`debug_assert!`s at every call site, active under the fuzzer's
//! debug-assertions build) — and otherwise asserts nothing at runtime;
//! survival plus ASan is the oracle.
//!
//! # Seed corpus and input budget
//!
//! `fuzz/seeds/operations/` holds checked-in seed inputs (`fuzz/corpus/` is
//! gitignored and cold-starts empty in CI, so seeds live outside it). Each
//! file decodes to one dependency-ordered sequence — lifecycle, burst, each
//! `Create*` before its dependents, the tearing/presentation trio — and
//! `fuzz.yml` passes the directory as an extra libFuzzer corpus argument.
//! Input length is capped by `-max_len` there; inside an op every count folds
//! to a small bound (`extra_rounds` to 0..=2, bursts to 1..=4 toplevels,
//! names to [`MAX_NAME_LEN`] chars, icon sizes to five entries), and every
//! pump is a bounded `Until::Turns` run, so the work per input is finite and
//! deterministic.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::cell::OnceCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// One compositor-side operation on a stateful wrapper.
///
/// # Scope: mostly reachable-without-a-client, plus the live toplevel lifecycle
///
/// This is the brief's `Operation` seed. The brief named `CreateToplevel`,
/// `CreatePopup`, `ConfigureToplevel` and `DestroyToplevel`. The *live-object*
/// forms of the popup half still need a connected wayland client driving popup
/// protocol and are rejected here: a real `Popup` exists only while such a
/// client is connected. Where a client-free *dangling-id miss* form exists it
/// is kept rather than omitted — [`ConfigureToplevel`] is the by-id
/// `Runtime::configure_toplevel` call on an id that can only miss (see the
/// by-id contract below), not a live reconfiguration of a client's toplevel.
/// Per the task rule ("a variant whose API you cannot drive yet should be
/// omitted rather than left as a no-op stub"), the committed set otherwise
/// covers what *is* reachable from Rust:
///
/// * **manager/global double-create guards** — `CreateXdgShell`,
///   `CreateLayerShell`, `CreateActivationManager`,
///   `CreateSessionLockManager`, `CreateTextInputManager`,
///   `CreateInputMethodManager`, `CreateOutputManager`;
/// * **the shared by-id lookup/miss contract** — every `SetToplevel*`,
///   `Popup*`, layer and output operation resolves a `*Id` through the same
///   id-table path and must miss cleanly on an unknown one;
/// * **state queries** — `QuerySessionLocked`, `InputMethodActive`, the IME
///   snapshots, `ScheduleFrameAll`;
/// * **the live toplevel lifecycle** — `ClientToplevelLifecycle` and
///   `ClientToplevelBurst` drive a real same-thread `wayland-client`
///   connection through create/commit/ack/destroy, reaching the announcement,
///   commit, configure, ack and destroy machinery with live objects.
///
/// The enum is cumulative: later state machines append their client-driven
/// operations here.
///
/// Field names name the argument, not the C call: `nth` selects a reserved
/// dangling id for the by-id mutators, and carries no meaning beyond giving a
/// sequence several distinct unknown ids. `name`/`handle` carry a
/// fuzzer-chosen token or workspace name, folded to [`MAX_NAME_LEN`] chars
/// (the fixed `"fuzz-*"` values stay on as seed-corpus inputs); `variant`
/// selects one of a few bounded icon-size lists, covering `&[]`,
/// `&[16, 32, 64]` and extremes.
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
    /// `Runtime::lock_surface` against a dangling surface id. A live lock
    /// surface needs a connected locker client, so this stays on the miss path.
    LockSurfaceOf { nth: u64 },
    /// `Runtime::create_security_context_manager`; double-create guard included.
    CreateSecurityContextManager,
    /// `Runtime::lookup_security_context` for a null client — the documented
    /// no-client miss, refused before any wlroots call. A live client needs a
    /// connected sandbox.
    LookupSecurityContext,
    /// `Runtime::create_fixes`; double-create guard included.
    CreateFixes,

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

    // --- xdg dialog / system bell / foreign (M9): manager create guards and
    // the client-free ownership paths ---
    /// `Runtime::create_xdg_dialog_manager`; double-create guard included.
    CreateXdgDialogManager,
    /// `Runtime::create_xdg_system_bell`; double-create guard included.
    CreateXdgSystemBell,
    /// `Runtime::create_xdg_toplevel_icon_manager`; double-create guard
    /// included. A client-driven `set_icon` needs a connected client, so the
    /// event path is not reachable here.
    CreateXdgToplevelIconManager,
    /// `Runtime::set_toplevel_icon_sizes` with a fuzzer-chosen size list,
    /// folded to a small bound: 0 selects `&[]`, 1 selects `&[16, 32, 64]`,
    /// 2 selects extremes (`&[0, -1, 1, i32::MAX, i32::MIN]`), and any other
    /// value selects a two-entry list derived from the byte itself. A no-op
    /// until the icon manager exists, and free of any client. The fixed
    /// preference list stays on as a seed-corpus input.
    SetToplevelIconSizes { variant: u8 },
    /// `Runtime::create_xdg_toplevel_tag_manager`; double-create guard
    /// included. As the icon manager, the tag/description event paths need a
    /// connected client.
    CreateXdgToplevelTagManager,
    /// `Runtime::create_xdg_foreign_registry`; double-create guard included.
    CreateForeignRegistry,
    /// `Runtime::create_xdg_foreign_v1`; misses until a registry exists, and
    /// exercises the double-create guard afterwards.
    CreateForeignV1,
    /// `Runtime::create_xdg_foreign_v2`; as `CreateForeignV1`.
    CreateForeignV2,
    /// `Runtime::add_activation_token` under a fuzzer-chosen name (folded to
    /// [`MAX_NAME_LEN`] chars; an interior NUL is refused, never truncated),
    /// then drop. Mints and releases a token without a client, exercising the
    /// owned handle's destroy path; a miss when no activation manager exists.
    /// The fixed `"fuzz-token"` name stays on as a seed-corpus input.
    AddActivationToken { name: String },
    /// `Runtime::find_activation_token` for a fuzzer-chosen name (folded like
    /// above); a miss unless a live token holds that name. This target never
    /// keeps a token handle alive, so every lookup here misses.
    FindActivationToken { name: String },
    /// `Runtime::find_foreign_exported` for a fuzzer-chosen handle (folded
    /// like above); a miss unless a live export holds that handle. Exports
    /// are dropped immediately, so every lookup here misses.
    FindForeignExported { handle: String },
    /// `Runtime::export_foreign` against a fuzzer-chosen dangling toplevel id,
    /// then drop. A live toplevel needs a connected client, so this stays on
    /// the miss path; it exercises the id lookup and the null-guarded return.
    ExportForeign { nth: u64 },

    // --- Activation token mint + presentation wiring: the client-free
    // owned-handle paths ---
    /// `Runtime::create_activation_token` then drop. The activation manager is
    /// created idempotently first (the pattern `connect_live_client` uses for
    /// `create_xdg_shell`), so the mint path runs regardless of input order;
    /// the handle's `Drop` runs here, exercising the destroy path under ASan.
    CreateActivationToken,
    /// `Runtime::create_presentation` (double-create guard included) followed
    /// by `Runtime::set_scene_presentation`, the call every scene compositor
    /// makes after creating presentation. Both are free of any client.
    CreatePresentation,
    /// `Runtime::dialog_of` against a dangling surface id plus
    /// `Runtime::dialog` against a dangling toplevel id. A live dialog needs
    /// a connected client, so both stay on the miss path here; the live-id
    /// half additionally runs in `stale_miss_subset` against the announced id.
    DialogOf { nth: u64, surface_nth: u64 },

    // --- Foreign-toplevel management (M9): manager create guard and the
    // owned handle's client-free lifecycle ---
    /// `Runtime::create_foreign_toplevel_manager`; double-create guard included.
    CreateForeignToplevelManager,
    /// Create two owned handles, drive every mutator, set one as the other's
    /// parent, and drop them in the order the input picks. No client is needed:
    /// the requests flow the other direction. Exercises the handle's destroy
    /// path under ASan, including wlroots' parent-rewrite on destroy. The
    /// manager is created idempotently first, so the live path runs regardless
    /// of input order; without it both creates miss and the op is a no-op.
    ForeignToplevelHandles { parent_first: bool },

    // --- ext-foreign-toplevel-list (M9): manager create guard and the owned
    // handle's client-free lifecycle ---
    /// `Runtime::create_ext_foreign_toplevel_list`; double-create guard
    /// included. The list is observation-only, so no client is needed to drive
    /// the handle's export/update/destroy path.
    CreateExtForeignToplevelList,
    /// Create two owned handles, set and update their state, and drop them in
    /// the order the input picks. Exercises the handle's destroy path under
    /// ASan. The list is created idempotently first, so the live path runs
    /// regardless of input order; without it both creates miss.
    ExtForeignToplevelHandles { first_first: bool },

    // --- ext-workspace (M9): manager create guard and the owned
    // group/workspace lifecycle ---
    /// `Runtime::create_ext_workspace_manager`; double-create guard included.
    CreateExtWorkspaceManager,
    /// Create a group and two workspaces under fuzzer-chosen ids (folded to
    /// [`MAX_NAME_LEN`] chars), assign one to the group, drive every
    /// workspace mutator, then drop the group and workspaces in the order the
    /// input picks. No client is needed: the commit requests flow the other
    /// direction. Exercises both destroy paths under ASan, including wlroots'
    /// group-rewrite of the workspace's group pointer. The manager is created
    /// idempotently first, so the live path runs regardless of input order.
    ExtWorkspaceHandles {
        group_first: bool,
        name_a: String,
        name_b: String,
    },

    // --- wlr_surface operations (M9b): `surface.rs` by-id reads and mutators.
    // A live `wlr_surface` needs a connected client, so these stay on the
    // surface lookup's miss path; the wrappers are covered positively by the
    // client-driven `tests/surfaces.rs`.
    /// `Surface::{extents,effective_damage,buffer_source_box,point_accepts_input,surface_at}`.
    SurfaceProbe { nth: u64, x: f64, y: f64 },
    /// `Surface::root_id`.
    SurfaceRoot { nth: u64 },
    /// `Surface::accepts_touch`.
    SurfaceAcceptsTouch { nth: u64 },
    /// `Surface::{lock_pending,unlock_cached}`.
    SurfaceLockPending { nth: u64 },
    /// `Surface::unmap`.
    SurfaceUnmap { nth: u64 },

    // --- layer-shell remainder (M9b): `runtime.rs` by-id destroy. A live
    // layer surface needs a connected client, so this stays on the miss path.
    /// `Runtime::destroy_layer_surface`.
    DestroyLayerSurface,

    // --- Live toplevel lifecycle: a real same-thread `wayland-client`
    // connection against the harness socket. These mint REAL toplevels —
    // `wl_surface` + `xdg_surface` + `xdg_toplevel`, committed, configured,
    // acked, destroyed — so the announcement/commit/configure/ack/destroy
    // machinery runs on live objects under ASan. See `drive_client_lifecycle`
    // and `drive_client_burst` for the interleaving (client flush / server
    // `run_all` turns / client
    // `dispatch_pending`); every count below is folded to a small bound, so
    // the work per input is deterministic and finite.
    /// Create one live toplevel, commit it, pump the server so it is
    /// announced and configured, ack the configures, then destroy/unmap it.
    /// `extra_rounds` folds to 0..=2 further commit/pump/ack rounds on the
    /// same surface; `requests` selects extra client requests deterministically
    /// (bit 0: `set_title`, bit 1: `set_app_id`, other bits reserved and
    /// ignored); `destroy` picks role-destroy-then-disconnect (true) versus
    /// disconnect-destroys (false). Afterwards a deterministic subset of the
    /// by-id toplevel mutators runs against the captured id, which by then
    /// observes the documented stale-miss boundary.
    ClientToplevelLifecycle {
        extra_rounds: u8,
        requests: u8,
        destroy: bool,
    },
    /// Mint and destroy several live toplevels on one connection, pumping
    /// between each, so announce/destroy churn runs with several objects alive
    /// at once. `count` folds to 1..=4 toplevels; `destroy_each` picks
    /// destroy-after-each-pump (true) versus destroy-all-at-the-end (false).
    ClientToplevelBurst { count: u8, destroy_each: bool },
}

/// The one handler this target installs.
///
/// `Backend::run_all` calls it from underneath wlroots' `extern "C"` frames, so
/// every method here is deliberately inert: `new_output`/`new_toplevel` store
/// the announced id (a plain `Option` assignment) and nothing else does
/// anything. No assert, no unwrap, no index, no re-entry into `wlr`.
#[derive(Default)]
struct Recorder {
    output: Option<wlr::OutputId>,
    toplevel: Option<wlr::ToplevelId>,
    turns: u32,
}

impl wlr::OutputHandler for Recorder {
    fn new_output(&mut self, output: &wlr::Output<'_>) {
        if self.output.is_none() {
            self.output = Some(output.id());
        }
    }
}

impl wlr::ToplevelHandler for Recorder {
    fn new_toplevel(&mut self, toplevel: &wlr::Toplevel<'_>) {
        if self.toplevel.is_none() {
            self.toplevel = Some(toplevel.id());
        }
    }
}
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
    /// Socket name from `Display::add_socket_auto`, resolved against
    /// `XDG_RUNTIME_DIR` by the client-driven ops. `None` when the socket
    /// could not be bound; those ops then skip the input deterministically.
    socket: Option<String>,
    /// The event loop the backend was created from, kept so the process-lifetime
    /// `Box::leak` behind it stays reachable (and LSan-clean): `Backend` only
    /// retains the raw loop pointer, so without this field nothing points at
    /// the box.
    _event_loop: &'static wlr::EventLoop<'static>,
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

            // The server binds its socket under `XDG_RUNTIME_DIR`. When the
            // fuzzer runs without one, point it at a per-process directory
            // derived only from the pid (no wall-clock, no randomness), so
            // the client-driven ops have a path to connect to.
            let runtime_dir_ok = std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| {
                !dir.is_empty() && std::path::Path::new(&dir).is_dir()
            });
            if !runtime_dir_ok {
                let dir = std::env::temp_dir().join(format!("wlr-fuzz-{}", std::process::id()));
                if std::fs::create_dir_all(&dir).is_ok() {
                    std::env::set_var("XDG_RUNTIME_DIR", &dir);
                }
            }

            let display: &'static wlr::Display = Box::leak(Box::new(
                wlr::Display::new().unwrap_or_else(|e| panic!("fuzz harness could not start: {e}")),
            ));
            let event_loop: &'static wlr::EventLoop<'static> =
                Box::leak(Box::new(display.event_loop()));
            let backend = wlr::Backend::autocreate(event_loop)
                .unwrap_or_else(|e| panic!("fuzz harness could not start: {e}"));
            let runtime =
                wlr::Runtime::new().unwrap_or_else(|e| panic!("fuzz harness could not start: {e}"));
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

            // Best-effort socket for the client-driven ops. A failure leaves
            // `None` and those ops skip their input; it never fails bring-up.
            let socket = display.add_socket_auto().ok();

            Some(Box::leak(Box::new(Compositor {
                runtime,
                output: recorder.output,
                socket,
                _event_loop: event_loop,
                _backend: backend,
                _display: display,
            })))
        })
    })
}

/// Process-wide client-attempt accounting: the silent-skip guard.
///
/// A broken socket path makes every client op skip its input, which reads as
/// a green run with zero live coverage. `connect_client` bumps `ATTEMPTED` on
/// every call and `CONNECTED` on every successful connect; both reset at each
/// fuzz-input start, and the end of the input panics when attempts happened
/// but nothing ever connected — failing the run like a bring-up failure. A
/// healthy socket never trips this: every attempt connects.
static ATTEMPTED: AtomicUsize = AtomicUsize::new(0);
static CONNECTED: AtomicUsize = AtomicUsize::new(0);

fuzz_target!(|ops: Vec<Operation>| {
    ATTEMPTED.store(0, Ordering::Relaxed);
    CONNECTED.store(0, Ordering::Relaxed);
    let Some(compositor) = compositor() else { return; };
    for op in &ops {
        apply(compositor, op);
    }
    // No live client for the whole input means the live ops all skipped: fail
    // the run rather than banking a green input that fuzzed nothing live.
    let attempted = ATTEMPTED.load(Ordering::Relaxed);
    let connected = CONNECTED.load(Ordering::Relaxed);
    assert!(
        attempted == 0 || connected > 0,
        "fuzz harness produced no live client: \
         {attempted} connect attempts, {connected} successes"
    );
});

/// Fold a fuzzer-supplied name to a small bound.
///
/// `Arbitrary` strings are unbounded; wlroots copies the bytes it needs at
/// creation, so length only buys allocator work, never coverage. Truncating
/// to [`MAX_NAME_LEN`] chars keeps the work per input bounded while still
/// covering empty, interior-NUL (refused by `CString::new`, never truncated)
/// and multi-byte shapes. The fixed `"fuzz-*"` values stay on as
/// seed-corpus inputs.
const MAX_NAME_LEN: usize = 24;

/// See [`MAX_NAME_LEN`].
fn fold_name(name: &str) -> String {
    name.chars().take(MAX_NAME_LEN).collect()
}

/// Assert the frozen unknown-id-miss contract.
///
/// A reserved-band dangling id (or a stale id past its announcing pump) never
/// resolves, so the call must report a miss. A `Some` here means the wrapper
/// resolved an id it could not own — the memory-safety boundary this target
/// exists to watch. The integration tests named in the header docs are the
/// logic oracle for the positive paths; this pins the miss half under ASan.
fn expect_miss<T>(result: Option<T>, what: &'static str) {
    debug_assert!(
        result.is_none(),
        "frozen contract violated: `{what}` resolved an id that must miss"
    );
}

/// Assert a miss reported as an empty collection (`popups_of`, `popup_chain`:
/// a dangling parent has no children).
fn expect_empty<T>(items: &[T], what: &'static str) {
    debug_assert!(
        items.is_empty(),
        "frozen contract violated: `{what}` listed children of an id that must miss"
    );
}

/// Assert a miss reported as a zero destroy count (`dismiss_popup`,
/// `dismiss_popups_of`: nothing live under a dangling id).
fn expect_zero(destroyed: usize, what: &'static str) {
    debug_assert!(
        destroyed == 0,
        "frozen contract violated: `{what}` destroyed under an id that must miss"
    );
}

/// Assert a miss reported as `false` (`configure_popup`, `popup_is_grabbing`).
fn expect_false(hit: bool, what: &'static str) {
    debug_assert!(
        !hit,
        "frozen contract violated: `{what}` hit an id that must miss"
    );
}

/// Assert the frozen double-create-refusal contract.
///
/// The first create in each arm may succeed (fresh process) or fail (an
/// earlier input already created the global); either way a second immediate
/// create must be refused, because no state changed in between except the
/// first call itself. A succeeding second call means the guard regressed.
fn expect_double_create_refused<T, E>(second: Result<T, E>, what: &'static str) {
    debug_assert!(
        second.is_err(),
        "frozen contract violated: second `{what}` create succeeded"
    );
}

/// Drive one operation. Results that carry no contract are discarded; results
/// that do feed the frozen-contract `debug_assert!`s (`expect_miss`,
/// `expect_double_create_refused` and friends) — the only runtime assertions
/// in the target. Beyond those, its oracle is that the process survives and
/// ASan sees no invalid access.
fn apply(compositor: &Compositor, op: &Operation) {
    use wlr::{
        Box2D, DecorationMode, ExtForeignToplevelState, LayerSurfaceId, PopupId, PopupParent,
        SurfaceId, ToplevelId, WmCapabilities, WorkspaceCapabilities, WorkspaceGroupCapabilities,
    };

    let runtime = &compositor.runtime;
    let display: &wlr::Display = compositor._display;
    let output = compositor.output;

    let toplevel = |nth: u64| ToplevelId::dangling_nth_for_test(nth);
    let popup = |nth: u64| PopupId::dangling_nth_for_test(nth);

    match op {
        Operation::CreateXdgShell => {
            let _ = runtime.create_xdg_shell(display, 6);
            expect_double_create_refused(runtime.create_xdg_shell(display, 6), "create_xdg_shell");
        }

        Operation::SetToplevelSize { nth, width, height } => {
            expect_miss(
                runtime.set_toplevel_size(toplevel(*nth), *width, *height),
                "set_toplevel_size",
            );
        }
        Operation::SetToplevelActivated { nth, activated } => {
            expect_miss(
                runtime.set_toplevel_activated(toplevel(*nth), *activated),
                "set_toplevel_activated",
            );
        }
        Operation::SetToplevelMaximized { nth, maximized } => {
            expect_miss(
                runtime.set_toplevel_maximized(toplevel(*nth), *maximized),
                "set_toplevel_maximized",
            );
        }
        Operation::SetToplevelFullscreen { nth, fullscreen } => {
            expect_miss(
                runtime.set_toplevel_fullscreen(toplevel(*nth), *fullscreen),
                "set_toplevel_fullscreen",
            );
        }
        Operation::SetToplevelPosition { nth, x, y } => {
            expect_miss(
                runtime.set_toplevel_position(toplevel(*nth), *x, *y),
                "set_toplevel_position",
            );
        }
        Operation::SetToplevelVisible { nth, visible } => {
            expect_miss(
                runtime.set_toplevel_visible(toplevel(*nth), *visible),
                "set_toplevel_visible",
            );
        }
        Operation::RaiseToplevel { nth } => {
            expect_miss(runtime.raise_toplevel(toplevel(*nth)), "raise_toplevel");
        }
        Operation::ConfigureToplevel { nth } => {
            expect_miss(
                runtime.configure_toplevel(toplevel(*nth)),
                "configure_toplevel",
            );
        }
        Operation::CloseToplevel { nth } => {
            expect_miss(runtime.close_toplevel(toplevel(*nth)), "close_toplevel");
        }
        Operation::SetToplevelBounds { nth, width, height } => {
            expect_miss(
                runtime.set_toplevel_bounds(toplevel(*nth), *width, *height),
                "set_toplevel_bounds",
            );
        }
        Operation::SetToplevelConstrained {
            nth,
            top,
            bottom,
            left,
            right,
        } => {
            expect_miss(
                runtime.set_toplevel_constrained(
                    toplevel(*nth),
                    wlr::Edges {
                        top: *top,
                        bottom: *bottom,
                        left: *left,
                        right: *right,
                    },
                ),
                "set_toplevel_constrained",
            );
        }
        Operation::SetToplevelParent { nth, parent_nth } => {
            expect_miss(
                runtime.set_toplevel_parent(toplevel(*nth), Some(toplevel(*parent_nth))),
                "set_toplevel_parent",
            );
        }
        Operation::SetToplevelResizing { nth, resizing } => {
            expect_miss(
                runtime.set_toplevel_resizing(toplevel(*nth), *resizing),
                "set_toplevel_resizing",
            );
        }
        Operation::SetToplevelSuspended { nth, suspended } => {
            expect_miss(
                runtime.set_toplevel_suspended(toplevel(*nth), *suspended),
                "set_toplevel_suspended",
            );
        }
        Operation::SetToplevelTiled {
            nth,
            top,
            bottom,
            left,
            right,
        } => {
            expect_miss(
                runtime.set_toplevel_tiled(
                    toplevel(*nth),
                    wlr::Edges {
                        top: *top,
                        bottom: *bottom,
                        left: *left,
                        right: *right,
                    },
                ),
                "set_toplevel_tiled",
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
            expect_miss(
                runtime.set_toplevel_wm_capabilities(toplevel(*nth), c),
                "set_toplevel_wm_capabilities",
            );
        }
        Operation::DecorationState { nth } => {
            expect_miss(runtime.decoration_state(toplevel(*nth)), "decoration_state");
        }
        Operation::DecorationConfigure { nth } => {
            expect_miss(
                runtime.decoration_configure(toplevel(*nth)),
                "decoration_configure",
            );
        }
        Operation::SetDecorationMode { nth, server_side } => {
            let mode = if *server_side {
                DecorationMode::ServerSide
            } else {
                DecorationMode::ClientSide
            };
            expect_miss(
                runtime.set_decoration_mode(toplevel(*nth), mode),
                "set_decoration_mode",
            );
        }
        Operation::FocusToplevelKeyboard { nth } => {
            expect_miss(
                runtime.focus_toplevel_keyboard(toplevel(*nth)),
                "focus_toplevel_keyboard",
            );
        }
        Operation::ToplevelAt { x, y } => {
            let _ = runtime.toplevel_at(*x, *y);
        }

        Operation::PopupParentOf { nth } => {
            expect_miss(runtime.popup_parent(popup(*nth)), "popup_parent");
        }
        Operation::PopupsOf { nth } => {
            expect_empty(
                &runtime.popups_of(PopupParent::Popup(popup(*nth))),
                "popups_of",
            );
        }
        Operation::PopupChain { nth } => {
            expect_empty(
                &runtime.popup_chain(PopupParent::Popup(popup(*nth))),
                "popup_chain",
            );
        }
        Operation::ConfigurePopup {
            nth,
            x,
            y,
            width,
            height,
        } => {
            expect_false(
                runtime.configure_popup(popup(*nth), &Box2D::new(*x, *y, *width, *height)),
                "configure_popup",
            );
        }
        Operation::PopupPosition { nth } => {
            expect_miss(runtime.popup_position(popup(*nth)), "popup_position");
        }
        Operation::DismissPopup { nth } => {
            expect_zero(runtime.dismiss_popup(popup(*nth)), "dismiss_popup");
        }
        Operation::DismissPopupsOf { nth } => {
            expect_zero(
                runtime.dismiss_popups_of(PopupParent::Popup(popup(*nth))),
                "dismiss_popups_of",
            );
        }
        Operation::PopupIsGrabbing { nth } => {
            expect_false(runtime.popup_is_grabbing(popup(*nth)), "popup_is_grabbing");
        }

        Operation::CreateLayerShell => {
            let _ = runtime.create_layer_shell(display, 4);
            expect_double_create_refused(
                runtime.create_layer_shell(display, 4),
                "create_layer_shell",
            );
        }
        Operation::ConfigureLayerSurface { width, height } => {
            expect_miss(
                runtime.configure_layer_surface(
                    LayerSurfaceId::dangling_for_test(),
                    *width,
                    *height,
                ),
                "configure_layer_surface",
            );
        }
        Operation::SetLayerSurfacePosition { x, y } => {
            expect_miss(
                runtime.set_layer_surface_position(LayerSurfaceId::dangling_for_test(), *x, *y),
                "set_layer_surface_position",
            );
        }
        Operation::FocusLayerKeyboard => {
            expect_miss(
                runtime.focus_layer_keyboard(LayerSurfaceId::dangling_for_test()),
                "focus_layer_keyboard",
            );
        }
        Operation::SetLayerSurfaceOutput => {
            if let Some(output) = output {
                expect_miss(
                    runtime.set_layer_surface_output(LayerSurfaceId::dangling_for_test(), output),
                    "set_layer_surface_output",
                );
            }
        }

        Operation::CreateActivationManager => {
            let _ = runtime.create_xdg_activation_manager(display);
            expect_double_create_refused(
                runtime.create_xdg_activation_manager(display),
                "create_xdg_activation_manager",
            );
        }

        Operation::CreateSessionLockManager => {
            let _ = runtime.create_session_lock_manager(display);
            expect_double_create_refused(
                runtime.create_session_lock_manager(display),
                "create_session_lock_manager",
            );
        }
        Operation::QuerySessionLocked => {
            let _ = runtime.is_session_locked();
        }
        Operation::LockSurfaceOf { nth } => {
            expect_miss(
                runtime.lock_surface(SurfaceId::dangling_nth_for_test(*nth)),
                "lock_surface",
            );
        }

        Operation::CreateSecurityContextManager => {
            let _ = runtime.create_security_context_manager(display);
            expect_double_create_refused(
                runtime.create_security_context_manager(display),
                "create_security_context_manager",
            );
        }
        Operation::LookupSecurityContext => {
            // SAFETY: null is the explicit "no client" case, refused before
            // any wlroots call.
            expect_miss(
                unsafe { runtime.lookup_security_context(std::ptr::null()) },
                "lookup_security_context",
            );
        }
        Operation::CreateFixes => {
            let _ = runtime.create_fixes(display, 1);
            expect_double_create_refused(runtime.create_fixes(display, 1), "create_fixes");
        }

        Operation::CreateTextInputManager => {
            let _ = runtime.create_text_input_manager(display);
            expect_double_create_refused(
                runtime.create_text_input_manager(display),
                "create_text_input_manager",
            );
        }
        Operation::CreateInputMethodManager => {
            let _ = runtime.create_input_method_manager(display);
            expect_double_create_refused(
                runtime.create_input_method_manager(display),
                "create_input_method_manager",
            );
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
            expect_double_create_refused(
                runtime.create_output_manager(display),
                "create_output_manager",
            );
        }
        Operation::UpdateOutputManagerState => {
            runtime.update_output_manager_state();
        }
        Operation::ScheduleFrameAll => {
            let _ = runtime.schedule_frame_all();
        }
        Operation::OutputLayoutBox => {
            if let Some(output) = output {
                expect_miss(runtime.output_layout_box(output), "output_layout_box");
            }
        }
        Operation::SetOutputPosition { x, y } => {
            if let Some(output) = output {
                expect_miss(
                    runtime.set_output_position(output, *x, *y),
                    "set_output_position",
                );
            }
        }
        Operation::ScheduleFrame => {
            if let Some(output) = output {
                expect_miss(runtime.schedule_frame(output), "schedule_frame");
            }
        }

        Operation::CreateTearingControlManager => {
            let _ = runtime.create_tearing_control_manager(display, 1);
            expect_double_create_refused(
                runtime.create_tearing_control_manager(display, 1),
                "create_tearing_control_manager",
            );
        }
        Operation::SamplePresentation { nth } => {
            expect_miss(
                runtime.sample_presentation(SurfaceId::dangling_nth_for_test(*nth)),
                "sample_presentation",
            );
        }
        Operation::TearingHint { nth } => {
            expect_miss(
                runtime.tearing_hint(SurfaceId::dangling_nth_for_test(*nth)),
                "tearing_hint",
            );
        }
        Operation::TearingControlOf { nth } => {
            expect_miss(
                runtime.tearing_control(SurfaceId::dangling_nth_for_test(*nth)),
                "tearing_control",
            );
        }

        Operation::SubsurfaceParentId { nth } => {
            expect_miss(
                runtime
                    .surface(SurfaceId::dangling_nth_for_test(*nth))
                    .map(|surface| surface.subsurface_parent_id()),
                "subsurface_parent_id",
            );
        }
        Operation::SubsurfaceParentState { nth } => {
            expect_miss(
                runtime
                    .surface(SurfaceId::dangling_nth_for_test(*nth))
                    .map(|surface| surface.subsurface_parent_state()),
                "subsurface_parent_state",
            );
        }
        Operation::ToplevelOf { nth } => {
            expect_miss(
                runtime.toplevel_of(SurfaceId::dangling_nth_for_test(*nth)),
                "toplevel_of",
            );
        }
        Operation::PopupOf { nth } => {
            expect_miss(
                runtime.popup_of(SurfaceId::dangling_nth_for_test(*nth)),
                "popup_of",
            );
        }

        Operation::CreateXdgDialogManager => {
            let _ = runtime.create_xdg_dialog_manager(display, 1);
            expect_double_create_refused(
                runtime.create_xdg_dialog_manager(display, 1),
                "create_xdg_dialog_manager",
            );
        }
        Operation::CreateXdgSystemBell => {
            let _ = runtime.create_xdg_system_bell(display, 1);
            expect_double_create_refused(
                runtime.create_xdg_system_bell(display, 1),
                "create_xdg_system_bell",
            );
        }
        Operation::CreateXdgToplevelIconManager => {
            let _ = runtime.create_xdg_toplevel_icon_manager(display, 1);
            expect_double_create_refused(
                runtime.create_xdg_toplevel_icon_manager(display, 1),
                "create_xdg_toplevel_icon_manager",
            );
        }
        Operation::SetToplevelIconSizes { variant } => {
            // Folded to a small bound; the fixed preference list stays a seed
            // input, and `&[]`/extremes are covered deterministically.
            let _ = match variant % 4 {
                0 => runtime.set_toplevel_icon_sizes(&[]),
                1 => runtime.set_toplevel_icon_sizes(&[16, 32, 64]),
                2 => runtime.set_toplevel_icon_sizes(&[0, -1, 1, i32::MAX, i32::MIN]),
                _ => runtime.set_toplevel_icon_sizes(&[(*variant as i32).wrapping_mul(0x9E37), 16]),
            };
        }
        Operation::CreateXdgToplevelTagManager => {
            let _ = runtime.create_xdg_toplevel_tag_manager(display, 1);
            expect_double_create_refused(
                runtime.create_xdg_toplevel_tag_manager(display, 1),
                "create_xdg_toplevel_tag_manager",
            );
        }
        Operation::CreateForeignRegistry => {
            let _ = runtime.create_xdg_foreign_registry(display);
            expect_double_create_refused(
                runtime.create_xdg_foreign_registry(display),
                "create_xdg_foreign_registry",
            );
        }
        Operation::CreateForeignV1 => {
            let _ = runtime.create_xdg_foreign_v1(display);
            expect_double_create_refused(
                runtime.create_xdg_foreign_v1(display),
                "create_xdg_foreign_v1",
            );
        }
        Operation::CreateForeignV2 => {
            let _ = runtime.create_xdg_foreign_v2(display);
            expect_double_create_refused(
                runtime.create_xdg_foreign_v2(display),
                "create_xdg_foreign_v2",
            );
        }
        Operation::AddActivationToken { name } => {
            // The handle is dropped here, running its `Drop` immediately; a
            // miss (no manager) is discarded like every other result.
            let _ = runtime.add_activation_token(&fold_name(name));
        }
        Operation::FindActivationToken { name } => {
            expect_miss(
                runtime.find_activation_token(&fold_name(name)),
                "find_activation_token",
            );
        }
        Operation::FindForeignExported { handle } => {
            expect_miss(
                runtime.find_foreign_exported(&fold_name(handle)),
                "find_foreign_exported",
            );
        }
        Operation::ExportForeign { nth } => {
            expect_miss(runtime.export_foreign(toplevel(*nth)), "export_foreign");
        }

        Operation::CreateActivationToken => {
            let _ = runtime.create_xdg_activation_manager(display);
            // Mint + drop: the handle's `Drop` runs here, exercising the
            // destroy path under ASan. After the idempotent create above, a
            // miss is a genuine allocation failure only.
            let minted = runtime.create_activation_token();
            debug_assert!(
                minted.is_some(),
                "create_activation_token missed after its manager create: allocation failure"
            );
        }
        Operation::CreatePresentation => {
            let _ = runtime.create_presentation(display, &compositor._backend);
            expect_double_create_refused(
                runtime.create_presentation(display, &compositor._backend),
                "create_presentation",
            );
            // The call every scene compositor makes after creating
            // presentation; safe to repeat, free of any client.
            let _ = runtime.set_scene_presentation();
        }
        Operation::DialogOf { nth, surface_nth } => {
            expect_miss(
                runtime.dialog_of(SurfaceId::dangling_nth_for_test(*surface_nth)),
                "dialog_of",
            );
            expect_miss(runtime.dialog(toplevel(*nth)), "dialog");
        }

        Operation::CreateForeignToplevelManager => {
            let _ = runtime.create_foreign_toplevel_manager(display);
            expect_double_create_refused(
                runtime.create_foreign_toplevel_manager(display),
                "create_foreign_toplevel_manager",
            );
        }
        Operation::ForeignToplevelHandles { parent_first } => {
            // Idempotent prerequisite (the pattern `connect_live_client` uses
            // for `create_xdg_shell`): the double-create guard makes repeats a
            // no-op, so the live path runs regardless of input order. After
            // it, `None` is a genuine allocation failure only.
            let _ = runtime.create_foreign_toplevel_manager(display);
            let Some(a) = runtime.create_foreign_toplevel() else {
                debug_assert!(
                    false,
                    "create_foreign_toplevel missed after its manager create: allocation failure"
                );
                return;
            };
            let Some(b) = runtime.create_foreign_toplevel() else {
                debug_assert!(
                    false,
                    "create_foreign_toplevel missed after its manager create: allocation failure"
                );
                return;
            };
            let _ = a.set_title("fuzz");
            let _ = a.set_app_id("fuzz.app");
            let _ = a.set_maximized(true);
            let _ = a.set_minimized(true);
            let _ = a.set_activated(true);
            let _ = a.set_fullscreen(true);
            let _ = a.state();
            let _ = b.set_parent(Some(&a));
            // The order is the input's; both must be double-free safe.
            if *parent_first {
                drop(a);
                drop(b);
            } else {
                drop(b);
                drop(a);
            }
        }

        Operation::CreateExtForeignToplevelList => {
            let _ = runtime.create_ext_foreign_toplevel_list(display, 1);
            expect_double_create_refused(
                runtime.create_ext_foreign_toplevel_list(display, 1),
                "create_ext_foreign_toplevel_list",
            );
        }
        Operation::ExtForeignToplevelHandles { first_first } => {
            // Idempotent prerequisite, as above: after it, `None` is a
            // genuine allocation failure only.
            let _ = runtime.create_ext_foreign_toplevel_list(display, 1);
            // `ExtForeignToplevelState` is `#[non_exhaustive]`, so a downstream
            // crate cannot use a struct literal at all — build via `default()`
            // plus field assignment, as the integration tests do.
            let mut state_a = ExtForeignToplevelState::default();
            state_a.title = Some("fuzz-a".to_owned());
            state_a.app_id = Some("fuzz.app".to_owned());
            let Some(a) = runtime.create_ext_foreign_toplevel(&state_a) else {
                debug_assert!(
                    false,
                    "create_ext_foreign_toplevel missed after its list create: allocation failure"
                );
                return;
            };
            let mut state_b = ExtForeignToplevelState::default();
            state_b.title = Some("fuzz-b".to_owned());
            let Some(b) = runtime.create_ext_foreign_toplevel(&state_b) else {
                debug_assert!(
                    false,
                    "create_ext_foreign_toplevel missed after its list create: allocation failure"
                );
                return;
            };
            let _ = a.state();
            let _ = a.identifier();
            let mut state_a2 = ExtForeignToplevelState::default();
            state_a2.title = Some("fuzz-a2".to_owned());
            let _ = a.update_state(&state_a2);
            // The order is the input's; both must be double-free safe.
            if *first_first {
                drop(a);
                drop(b);
            } else {
                drop(b);
                drop(a);
            }
        }

        Operation::CreateExtWorkspaceManager => {
            let _ = runtime.create_ext_workspace_manager(display, 1);
            expect_double_create_refused(
                runtime.create_ext_workspace_manager(display, 1),
                "create_ext_workspace_manager",
            );
        }
        Operation::ExtWorkspaceHandles {
            group_first,
            name_a,
            name_b,
        } => {
            // Idempotent prerequisite, as above: after it, `None` is a
            // genuine allocation failure only.
            let _ = runtime.create_ext_workspace_manager(display, 1);
            let Some(group) = runtime
                .create_workspace_group(WorkspaceGroupCapabilities::CREATE_WORKSPACE)
            else {
                debug_assert!(
                    false,
                    "create_workspace_group missed after its manager create: allocation failure"
                );
                return;
            };
            let caps = WorkspaceCapabilities::ACTIVATE
                | WorkspaceCapabilities::DEACTIVATE
                | WorkspaceCapabilities::ASSIGN
                | WorkspaceCapabilities::REMOVE;
            let id_a = fold_name(name_a);
            let id_b = fold_name(name_b);
            let Some(a) = runtime.create_workspace(&id_a, caps) else {
                // An interior NUL is refused before any wlroots call (a
                // third miss reason besides the manager and allocation
                // ones); only a NUL-free miss is an allocation failure.
                debug_assert!(
                    id_a.contains('\0'),
                    "create_workspace missed after its manager create with a NUL-free id: \
                     allocation failure"
                );
                return;
            };
            let Some(b) = runtime.create_workspace(&id_b, caps) else {
                debug_assert!(
                    id_b.contains('\0'),
                    "create_workspace missed after its manager create with a NUL-free id: \
                     allocation failure"
                );
                return;
            };
            let _ = a.set_name(&id_a);
            let _ = a.set_coordinates(&[1, 2, 3]);
            let _ = a.set_active(true);
            let _ = a.set_urgent(true);
            let _ = a.set_hidden(true);
            let _ = a.set_group(Some(&group));
            let _ = b.set_name(&id_b);
            let _ = b.set_group(Some(&group));
            // The order is the input's; both must be double-free safe, and the
            // group's own destroy rewrites the workspaces' group pointers.
            if *group_first {
                drop(group);
                drop(a);
                drop(b);
            } else {
                drop(a);
                drop(b);
                drop(group);
            }
        }

        Operation::SurfaceProbe { nth, x, y } => {
            expect_miss(
                runtime
                    .surface(SurfaceId::dangling_nth_for_test(*nth))
                    .map(|surface| {
                        let _ = surface.extents();
                        let _ = surface.effective_damage();
                        let _ = surface.buffer_source_box();
                        let _ = surface.point_accepts_input(*x, *y);
                        let _ = surface.surface_at(*x, *y);
                    }),
                "surface probe",
            );
        }
        Operation::SurfaceRoot { nth } => {
            expect_miss(
                runtime
                    .surface(SurfaceId::dangling_nth_for_test(*nth))
                    .map(|surface| surface.root_id()),
                "surface root_id",
            );
        }
        Operation::SurfaceAcceptsTouch { nth } => {
            expect_miss(
                runtime
                    .surface(SurfaceId::dangling_nth_for_test(*nth))
                    .map(|surface| surface.accepts_touch()),
                "surface accepts_touch",
            );
        }
        Operation::SurfaceLockPending { nth } => {
            expect_miss(
                runtime
                    .surface(SurfaceId::dangling_nth_for_test(*nth))
                    .map(|surface| {
                        let lock = surface.lock_pending();
                        let _ = surface.unlock_cached(lock);
                    }),
                "surface lock_pending",
            );
        }
        Operation::SurfaceUnmap { nth } => {
            expect_miss(
                runtime
                    .surface(SurfaceId::dangling_nth_for_test(*nth))
                    .map(|surface| surface.unmap()),
                "surface unmap",
            );
        }
        Operation::DestroyLayerSurface => {
            expect_miss(
                runtime.destroy_layer_surface(LayerSurfaceId::dangling_for_test()),
                "destroy_layer_surface",
            );
        }

        Operation::ClientToplevelLifecycle {
            extra_rounds,
            requests,
            destroy,
        } => {
            drive_client_lifecycle(compositor, *extra_rounds, *requests, *destroy);
        }
        Operation::ClientToplevelBurst {
            count,
            destroy_each,
        } => {
            drive_client_burst(compositor, *count, *destroy_each);
        }
    }
}

/// Bound for the client socket's blocking reads/writes.
///
/// Every client call below is non-blocking (`flush` / `run_all(Turns)` /
/// non-blocking `read` / `dispatch_pending`) except the socket send/receive
/// itself, which this bounds: a stuck server turn surfaces as a timeout error
/// the driver ignores (skipping the input) instead of a hung fuzz run.
const CLIENT_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Pump the server without blocking: exactly `turns` zero-timeout turns, and
/// report the first toplevel announced during the pump.
///
/// Failures are ignored — the driver is best-effort; ASan is the oracle. The
/// id is stale by the time the pump returns (tables are per-run), so callers
/// use it only for the documented stale-miss boundary.
fn pump(compositor: &Compositor, turns: u32) -> Option<wlr::ToplevelId> {
    pump_capture(compositor, turns)
}

/// The pump itself: run the server and capture the announced toplevel.
/// [`pump`] is the same call with the id kept by the caller.
fn pump_capture(compositor: &Compositor, turns: u32) -> Option<wlr::ToplevelId> {
    let mut recorder = Recorder::default();
    let _ = compositor._backend.run_all(
        compositor._display,
        &mut recorder,
        &compositor.runtime,
        wlr::Until::Turns(turns),
    );
    recorder.toplevel
}

/// Client-side state for the synchronous fuzz client.
///
/// Only the registry list and nothing else is stored; configure events are
/// acked inline by the `Dispatch` impls (a protocol requirement, not `wlr`
/// re-entry — this runs on the client side, outside wlroots' `extern "C"`
/// frames).
#[derive(Default)]
struct FuzzClient {
    globals: Vec<(u32, String, u32)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for FuzzClient {
    fn event(
        state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for FuzzClient {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for FuzzClient {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for FuzzClient {
    fn event(
        _state: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for FuzzClient {
    fn event(
        _state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for FuzzClient {
    fn event(
        _state: &mut Self,
        _proxy: &xdg_toplevel::XdgToplevel,
        _event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// Resolve the compositor socket path, or `None` when there is no socket or
/// no runtime dir. Deterministic skip, never a panic.
fn socket_path(compositor: &Compositor) -> Option<std::path::PathBuf> {
    let name = compositor.socket.as_ref()?;
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    if dir.is_empty() {
        return None;
    }
    Some(std::path::Path::new(&dir).join(name))
}

/// Connect with bounded I/O. Any failure (no server, bad socket, timeout
/// setup) is a deterministic skip of the input.
fn connect_client(compositor: &Compositor) -> Option<std::os::unix::net::UnixStream> {
    ATTEMPTED.fetch_add(1, Ordering::Relaxed);
    let path = socket_path(compositor)?;
    let stream = std::os::unix::net::UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT)).ok()?;
    CONNECTED.fetch_add(1, Ordering::Relaxed);
    Some(stream)
}

/// Drain whatever the server has sent without blocking, then dispatch it.
/// `prepare_read` returns `None` when the inner queue needs dispatching;
/// `read` returns `WouldBlock` when nothing arrived — both are ignored.
fn read_and_dispatch(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<FuzzClient>,
    state: &mut FuzzClient,
) {
    if let Some(guard) = conn.prepare_read() {
        let _ = guard.read();
    }
    let _ = queue.dispatch_pending(state);
}

/// One flush/pump/read/dispatch turn. Never blocks: `flush` sends what fits
/// (bounded by the write timeout), `run_all(Turns)` never waits, `read` never
/// waits, `dispatch_pending` only drains what arrived. Returns the first
/// toplevel the pump announced, so drivers keep it across announce turns.
fn client_server_turn(
    compositor: &Compositor,
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<FuzzClient>,
    state: &mut FuzzClient,
) -> Option<wlr::ToplevelId> {
    let _ = conn.flush();
    let announced = pump(compositor, 4);
    read_and_dispatch(conn, queue, state);
    announced
}

/// A connected client with bound `wl_compositor` + `xdg_wm_base`, or `None`
/// when the server did not advertise them within the bounded registry wait.
/// Keeps the connection, queue, registry and bound globals alive together.
struct LiveClient {
    conn: Connection,
    queue: wayland_client::EventQueue<FuzzClient>,
    _registry: wl_registry::WlRegistry,
    compositor: wl_compositor::WlCompositor,
    wm_base: xdg_wm_base::XdgWmBase,
}

fn connect_live_client(compositor: &Compositor) -> Option<LiveClient> {
    // The toplevel role needs the server's xdg-shell global; create it
    // idempotently so the lifecycle works regardless of input order (the
    // double-create guard makes repeats a no-op miss).
    let _ = compositor
        .runtime
        .create_xdg_shell(compositor._display, 6);

    let stream = connect_client(compositor)?;
    let conn = Connection::from_socket(stream).ok()?;
    let mut queue: wayland_client::EventQueue<FuzzClient> = conn.new_event_queue();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());

    let mut state = FuzzClient::default();
    // Bounded registry wait: flush, let the server answer, drain. No
    // `roundtrip`/`blocking_dispatch` anywhere — those would block the
    // single thread until the server runs, which it cannot do mid-call.
    for _ in 0..10 {
        let _ = conn.flush();
        let _ = pump(compositor, 4);
        read_and_dispatch(&conn, &mut queue, &mut state);
        let has_compositor = state
            .globals
            .iter()
            .any(|(_, interface, _)| interface == "wl_compositor");
        let has_wm_base = state
            .globals
            .iter()
            .any(|(_, interface, _)| interface == "xdg_wm_base");
        if has_compositor && has_wm_base {
            break;
        }
    }

    let compositor_name = state
        .globals
        .iter()
        .find(|(_, interface, _)| interface == "wl_compositor")
        .map(|(name, _, version)| (*name, *version))?;
    let wm_base_name = state
        .globals
        .iter()
        .find(|(_, interface, _)| interface == "xdg_wm_base")
        .map(|(name, _, version)| (*name, *version))?;
    // Clamp to the versions this driver speaks; the server advertises at
    // least these when the manager exists. `bind` returns the proxy directly
    // (it only panics on a protocol mismatch, which the name/interface lookup
    // above rules out).
    let wl_compositor: wl_compositor::WlCompositor = registry.bind::<
        wl_compositor::WlCompositor,
        _,
        _,
    >(
        compositor_name.0, compositor_name.1.min(6), &qh, ()
    );
    let wm_base: xdg_wm_base::XdgWmBase =
        registry.bind::<xdg_wm_base::XdgWmBase, _, _>(wm_base_name.0, wm_base_name.1.min(6), &qh, ());
    // Drain the binds without blocking; failures just mean fewer events.
    let mut state = FuzzClient::default();
    for _ in 0..2 {
        let _ = client_server_turn(compositor, &conn, &mut queue, &mut state);
    }
    Some(LiveClient {
        conn,
        queue,
        _registry: registry,
        compositor: wl_compositor,
        wm_base,
    })
}

/// After the live driving, issue a deterministic subset of the by-id toplevel
/// mutators against the id the pumps announced (stale by design) or a
/// dangling id when nothing was announced. Either way this observes the
/// documented stale-miss boundary under ASan.
fn stale_miss_subset(compositor: &Compositor, id: Option<wlr::ToplevelId>) {
    use wlr::ToplevelId;
    let id = id.unwrap_or_else(|| ToplevelId::dangling_nth_for_test(0));
    let runtime = &compositor.runtime;
    // Every id here is stale by design (its announcing pump returned) or
    // dangling, so each call must take the documented stale-miss path.
    expect_miss(
        runtime.set_toplevel_size(id, 800, 600),
        "stale set_toplevel_size",
    );
    expect_miss(
        runtime.set_toplevel_activated(id, true),
        "stale set_toplevel_activated",
    );
    expect_miss(runtime.configure_toplevel(id), "stale configure_toplevel");
    expect_miss(runtime.close_toplevel(id), "stale close_toplevel");
    expect_miss(runtime.dialog(id), "stale dialog");
}

/// Create one live toplevel, commit it, pump so it is announced and
/// configured, ack the configures, then destroy/unmap.
///
/// `extra_rounds` folds to 0..=2 further commit/pump/ack rounds;
/// `requests` bit 0 sends `set_title`, bit 1 sends `set_app_id`;
/// `destroy` picks role-destroy-then-disconnect versus disconnect-destroys.
/// A client that cannot connect skips the input.
fn drive_client_lifecycle(
    compositor: &Compositor,
    extra_rounds: u8,
    requests: u8,
    destroy: bool,
) {
    let extra_rounds = (extra_rounds % 3) as usize;
    let Some(mut client) = connect_live_client(compositor) else {
        return;
    };
    let qh = client.queue.handle();
    let mut state = FuzzClient::default();
    // The first id announced across these turns is kept. Every announcing
    // `pump` used to discard its Recorder while a trailing `pump_capture`
    // ran after the objects were gone; keeping the first `Some` here fixes
    // that staleness gap. The id is stale by design (its pump returned),
    // which is exactly the boundary the trailing by-id subset exercises.
    let mut announced: Option<wlr::ToplevelId> = None;

    let surface = client.compositor.create_surface(&qh, ());
    let xdg_surface = client.wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    if requests & 1 != 0 {
        toplevel.set_title("fuzz".to_owned());
    }
    if requests & 2 != 0 {
        toplevel.set_app_id("fuzz.app".to_owned());
    }
    surface.commit();
    // Announce + configure + ack: flush the role/commit, let the server run,
    // drain and ack whatever configures arrived. Repeated so a slow announce
    // still lands within the bound.
    for _ in 0..4 {
        announced = announced.or(client_server_turn(
            compositor,
            &client.conn,
            &mut client.queue,
            &mut state,
        ));
    }
    for _ in 0..extra_rounds {
        surface.commit();
        for _ in 0..2 {
            announced = announced.or(client_server_turn(
                compositor,
                &client.conn,
                &mut client.queue,
                &mut state,
            ));
        }
    }

    if destroy {
        toplevel.destroy();
        xdg_surface.destroy();
        surface.destroy();
        for _ in 0..3 {
            let _ = client_server_turn(compositor, &client.conn, &mut client.queue, &mut state);
        }
        drop((surface, xdg_surface, toplevel));
        drop(client);
        let _ = pump(compositor, 4);
    } else {
        // Disconnect destroys: drop the connection without role destroys and
        // let the server observe the disconnect.
        drop((surface, xdg_surface, toplevel));
        drop(client);
        let _ = pump(compositor, 4);
    }

    stale_miss_subset(compositor, announced);
}

/// Mint and destroy several live toplevels on one connection, pumping between
/// each. `count` folds to 1..=4; `destroy_each` picks
/// destroy-after-each-pump versus destroy-all-at-the-end.
fn drive_client_burst(compositor: &Compositor, count: u8, destroy_each: bool) {
    let count = ((count % 4) + 1) as usize;
    let Some(mut client) = connect_live_client(compositor) else {
        return;
    };
    let qh = client.queue.handle();
    let mut state = FuzzClient::default();
    // Kept across the announce turns, as in `drive_client_lifecycle`.
    let mut announced: Option<wlr::ToplevelId> = None;
    let mut live: Vec<(
        wl_surface::WlSurface,
        xdg_surface::XdgSurface,
        xdg_toplevel::XdgToplevel,
    )> = Vec::new();

    for _ in 0..count {
        let surface = client.compositor.create_surface(&qh, ());
        let xdg_surface = client.wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        for _ in 0..2 {
            announced = announced.or(client_server_turn(
                compositor,
                &client.conn,
                &mut client.queue,
                &mut state,
            ));
        }
        live.push((surface, xdg_surface, toplevel));
        if destroy_each {
            if let Some((surface, xdg_surface, toplevel)) = live.pop() {
                toplevel.destroy();
                xdg_surface.destroy();
                surface.destroy();
                for _ in 0..2 {
                    let _ =
                        client_server_turn(compositor, &client.conn, &mut client.queue, &mut state);
                }
            }
        }
    }
    for (surface, xdg_surface, toplevel) in live.drain(..) {
        toplevel.destroy();
        xdg_surface.destroy();
        surface.destroy();
    }
    for _ in 0..3 {
        let _ = client_server_turn(compositor, &client.conn, &mut client.queue, &mut state);
    }
    drop(client);
    let _ = pump(compositor, 4);

    stale_miss_subset(compositor, announced);
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
