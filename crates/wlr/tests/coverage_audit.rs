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
/// CI's all-features step sets `WLR_COVERAGE_ALL_FEATURES=1`.
#[test]
fn every_ledger_symbol_exists_in_bindings() {
    if std::env::var_os("WLR_COVERAGE_ALL_FEATURES").is_none() {
        eprintln!(
            "skipped: set WLR_COVERAGE_ALL_FEATURES=1 after building \
             `wlr-sys --all-features` to check for stale ledger rows"
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

/// bindgen skips C variadics silently, so one that *is* bound would mean the
/// generated file changed shape under us.
#[test]
fn no_bound_symbol_is_variadic() {
    let variadic = coverage::variadic_fns(&bindings_source());
    assert!(
        variadic.is_empty(),
        "bindgen bound a variadic wlr_* function, which it cannot call: {variadic:?}"
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
