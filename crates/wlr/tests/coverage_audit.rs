//! The coverage gate.
//!
//! Ground truth is the `bindings.rs` bindgen produced for this build, so a
//! wlroots patch release that adds a symbol fails CI on its own, without anyone
//! remembering to update a checklist. Every `wlr_*` symbol must appear in
//! exactly one of `coverage/wrapped.toml` and `coverage/waived.toml`.
//!
//! The audit *library* lives at `crates/wlr/src/coverage.rs` and is pulled in by
//! path rather than through `lib.rs`: the published crate must not carry audit
//! machinery, and `crates/xtask` includes the same file the same way so the gate
//! and the burn-down report cannot disagree about what "covered" means.
//!
//! Run `cargo build -p wlr-sys --all-features` first if the newest `bindings.rs`
//! under `target/` is not the configuration you meant to audit, or point
//! `WLR_COVERAGE_BINDINGS` at an exact file.

#[path = "../src/coverage.rs"]
mod coverage;

use std::path::{Path, PathBuf};

use coverage::{Report, WaiveReason, WaivedEntry, WrappedEntry};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

fn ledgers() -> (Vec<WrappedEntry>, Vec<WaivedEntry>) {
    let dir = manifest_dir().join("coverage");
    let wrapped_path = dir.join("wrapped.toml");
    let waived_path = dir.join("waived.toml");
    let wrapped = coverage::parse_wrapped(&read(&wrapped_path))
        .unwrap_or_else(|err| panic!("{}: {err}", wrapped_path.display()));
    let waived = coverage::parse_waived(&read(&waived_path))
        .unwrap_or_else(|err| panic!("{}: {err}", waived_path.display()));
    (wrapped, waived)
}

fn bindings_source() -> String {
    let manifest = manifest_dir();
    let root = coverage::workspace_root(&manifest).expect("workspace root");
    let path = coverage::find_bindings(&root).expect("generated bindings.rs");
    read(&path)
}

fn audit() -> Report {
    let (wrapped, waived) = ledgers();
    let symbols = coverage::extract(&bindings_source());
    assert!(
        symbols.len() > 500,
        "only {} symbols extracted; the bindings.rs shape has changed and the \
         extractor is silently scoring almost everything as missing",
        symbols.len()
    );
    let sys_uses = coverage::scan_sys_uses(&manifest_dir().join("src")).expect("scan src/");
    coverage::reconcile(&symbols, &wrapped, &waived, &sys_uses)
}

/// Whether these bindings came from an all-features build.
///
/// Asks for a symbol from each feature that gates a whole header, so a build
/// missing any one of them is not mistaken for the full set. These are
/// deliberately load-bearing names rather than a count: a count would drift
/// with every wlroots release and would have to be maintained, whereas a
/// symbol that disappears is a real event the audit should notice anyway.
fn is_all_features(bindings: &str) -> bool {
    const MARKERS: &[&str] = &[
        "wlr_vk_renderer_create_with_drm_fd",    // vulkan-renderer
        "wlr_gles2_renderer_create_with_drm_fd", // gles2-renderer
        "wlr_drm_backend_create",                // drm-backend
        "wlr_x11_backend_create",                // x11-backend
        "wlr_libinput_backend_create",           // libinput-backend
        "wlr_session_create",                    // session
    ];
    MARKERS.iter().all(|m| bindings.contains(m))
}

/// The marker set has to actually be findable, or `is_all_features` is a
/// permanent skip that looks like a pass — the exact failure it replaced.
///
/// This runs in whatever configuration the suite was invoked with, so it
/// cannot assert that the markers are *present*. It asserts the weaker thing
/// that still catches the real regression: a marker that no longer names
/// anything in a full build is a renamed or removed symbol, and the audit's
/// own `every_ledger_symbol_exists_in_bindings` would then silently stop
/// running. If these bindings *are* all-features, every marker must be there.
#[test]
fn the_all_features_markers_are_real_symbols() {
    let source = bindings_source();
    let found: Vec<&str> = [
        "wlr_vk_renderer_create_with_drm_fd",
        "wlr_gles2_renderer_create_with_drm_fd",
        "wlr_drm_backend_create",
        "wlr_x11_backend_create",
        "wlr_libinput_backend_create",
        "wlr_session_create",
    ]
    .into_iter()
    .filter(|m| source.contains(m))
    .collect();
    assert!(
        !found.is_empty(),
        "not one all-features marker appears in these bindings — every marker \
         has been renamed or removed, so is_all_features can never return true \
         and the stale-row gate would skip forever"
    );
    if is_all_features(&source) {
        assert_eq!(
            found.len(),
            6,
            "all-features build missing a marker: {found:?}"
        );
    }
}

/// The gate proper: nothing bound may be unaccounted for.
#[test]
fn every_wlr_symbol_is_wrapped_or_waived() {
    let report = audit();
    assert!(
        report.unlisted.is_empty(),
        "\n{}\nAdd each symbol to crates/wlr/coverage/wrapped.toml (with the safe \
         item that covers it) or waived.toml (with a reason).\n",
        report.render_failures()
    );
}

/// A `wrapped.toml` row whose symbol is no longer touched anywhere under
/// `crates/wlr/src` describes a wrapper that has been deleted or rewritten.
#[test]
fn no_wrapped_entry_is_stale() {
    let report = audit();
    assert!(
        report.stale_wrapped.is_empty(),
        "\n{}\n",
        report.render_failures()
    );
}

/// The mirror of [`no_wrapped_entry_is_stale`]: a `waived.toml` row still
/// tagged `not-yet` whose symbol `crates/wlr/src` already reaches.
///
/// It exists because a burn-down counted in one direction only reports
/// progress that is not real. Nothing outside this test forces a milestone to
/// move a row out of the backlog at the moment it wraps the symbol, and every
/// other check stays green when it forgets: the row is well-formed, the symbol
/// is bound, it appears in exactly one ledger. The only visible effect is that
/// `not-yet` keeps counting work that is already done, so the remaining
/// distance to 100% reads larger than it is and nobody has a reason to look.
/// Checking both directions is what makes the backlog a measurement.
#[test]
fn no_not_yet_entry_is_already_wrapped() {
    let report = audit();
    assert!(
        report.stale_not_yet.is_empty(),
        "\n{}\nMove each row from crates/wlr/coverage/waived.toml to \
         wrapped.toml, naming the module and the safe item that covers it.\n",
        report.render_failures()
    );
}

#[test]
fn no_symbol_is_in_both_ledgers() {
    let report = audit();
    assert!(
        report.duplicated.is_empty(),
        "\n{}\n",
        report.render_failures()
    );
}

/// A ledger row for a symbol bindgen never emitted is a typo or a symbol
/// wlroots removed.
///
/// Only a hard failure in the all-features configuration, which is where the
/// spec defines 100%: a smaller feature set produces a strict subset of the
/// symbols, so every ledger row for a gated symbol would otherwise fail there.
///
/// Which configuration this is, is read out of the bindings themselves rather
/// than out of an environment variable. It used to take
/// `WLR_COVERAGE_ALL_FEATURES=1`, and the failure mode was silent in the worst
/// way: copying CI's `run:` block without its separate `env:` key left every
/// stale ledger row unchecked while the test still reported ok. A gate whose
/// skip is indistinguishable from a pass is not a gate. The bindings know what
/// they contain, so they are asked.
#[test]
fn every_ledger_symbol_exists_in_bindings() {
    let source = bindings_source();
    if !is_all_features(&source) {
        eprintln!(
            "skipped: these bindings are not the all-features configuration \
             (no gated marker symbols present). Build `wlr-sys --all-features` \
             into an isolated CARGO_TARGET_DIR and point WLR_COVERAGE_BINDINGS \
             at its bindings.rs — a plain rebuild is often a cache hit that \
             leaves the newest bindings.rs belonging to another configuration."
        );
        return;
    }
    let report = audit();
    assert!(
        report.unknown_symbol.is_empty(),
        "\n{}\n",
        report.render_failures()
    );
}

/// The backlog is only a backlog if every row says who clears it.
#[test]
fn every_not_yet_entry_names_a_milestone() {
    let (_, waived) = ledgers();
    let missing: Vec<&str> = waived
        .iter()
        .filter(|w| w.reason == WaiveReason::NotYet && w.milestone.is_none())
        .map(|w| w.symbol.as_str())
        .collect();
    assert!(
        missing.is_empty(),
        "reason = \"not-yet\" requires a milestone:\n  {}\n",
        missing.join("\n  ")
    );
}

/// The variadics are pinned by name rather than merely counted.
///
/// `pub fn f(a: T, ...)` is legal Rust in an `extern "C"` block, so a C variadic
/// does reach `bindings.rs` — and wlroots 0.20 has exactly one. It cannot be
/// wrapped like its neighbours: Rust cannot forward arguments into a `va_list`,
/// so covering it means a hand-written call site that formats the message first
/// and passes a lone `%s`. A second one appearing inside a milestone that
/// budgeted for none is the failure this pins down.
#[test]
fn the_bound_variadics_are_the_known_ones() {
    let variadic = coverage::variadic_fns(&bindings_source());
    assert_eq!(
        variadic,
        vec!["wlr_surface_reject_pending".to_owned()],
        "the set of bound variadic wlr_* functions changed; a new one needs a \
         formatting call site, not an ordinary wrapper — see coverage/README.md"
    );
}

/// Un-ignored at M13. Passing this test is the definition of done.
#[test]
#[ignore = "the not-yet backlog is non-empty until M13"]
fn coverage_is_one_hundred_percent() {
    let report = audit();
    assert!(
        report.is_complete(),
        "{}\n{} symbol(s) still tagged not-yet",
        report.render_table(),
        report.not_yet.len()
    );
}
