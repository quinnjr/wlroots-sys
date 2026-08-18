//! The coverage ledger: extraction, parsing and reconciliation.
//!
//! This module answers one question mechanically — *which `wlr_*` symbols does
//! the safe crate cover?* — against three inputs: the `bindings.rs` bindgen
//! produced for this build, the two committed ledgers in `crates/wlr/coverage/`,
//! and a scan of the crate's own `sys::` uses.
//!
//! # Why this file is not a module of the library
//!
//! It is compiled twice, by `#[path]` include: once into
//! `crates/wlr/tests/coverage_audit.rs` (the CI gate) and once into
//! `crates/xtask/src/main.rs` (the burn-down report). Neither `lib.rs` nor the
//! published crate carries it — audit machinery is not API — and `xtask` must
//! not depend on `wlr`, which would drag a full wlroots build into a report that
//! only reads text files.
//!
//! The consequence is a hard constraint: **nothing here may name `crate::`, and
//! nothing here may use a dependency.** `std` only.
//!
//! # Why a line-oriented parser rather than `syn`
//!
//! bindgen's output is a fixed, rustfmt-normalised grammar — one item per block,
//! no `mod`, no macro soup — and the three "type" shapes are distinguished
//! entirely by the character following the name. A regex-free line pass over
//! 22.5k lines runs in milliseconds and is auditable in review, which matters
//! for a gate whose own silent drift is the failure mode being guarded against.
//! `syn` would need the same post-hoc filtering behind an AST indirection.
//!
//! # What this cannot prove
//!
//! `bindings.rs` shows only what bindgen *chose* to bind. A symbol that exists
//! in a wlroots header but that bindgen silently skipped is invisible here.
//! That risk belongs to `wlr-sys`' `tests/interop.rs`, not to this audit.
//! [`variadic_fns`] covers the neighbouring case — a symbol bindgen *did* bind
//! but which no ordinary wrapper can call the same way — because a ledger row
//! saying "wrapped" means something different for those.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// The shape of a bound symbol, as bindgen emitted it.
///
/// `Enum` is bindgen's newtype representation (`pub struct wlr_x(pub c_uint);`
/// plus associated constants), which is what wlr-sys generates — grepping for
/// `pub enum` finds nothing at all and would silently score every enum as
/// missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Fn,
    Enum,
    Struct,
    Union,
    TypeAlias,
}

impl Kind {
    /// The word used in ledger diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Fn => "fn",
            Kind::Enum => "enum",
            Kind::Struct => "struct",
            Kind::Union => "union",
            Kind::TypeAlias => "type",
        }
    }
}

/// One bound `wlr_*` symbol.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Symbol {
    pub name: String,
    pub kind: Kind,
}

/// Take the leading identifier of `s`, or `None` if `s` does not start with one.
fn leading_ident(s: &str) -> Option<(&str, &str)> {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    if end == 0 {
        None
    } else {
        Some(s.split_at(end))
    }
}

/// Parse a bindgen `bindings.rs` and return every public `wlr_*` symbol.
///
/// Anonymous nested aggregates (`*__bindgen_ty_N`) are excluded: they are
/// synthesised names for unnamed C unions and structs embedded in a parent, not
/// independently addressable API. bindgen's ~690 layout-assertion blocks are
/// excluded for free, because they begin `const _: () = {` and every pattern
/// here is anchored on `pub `.
pub fn extract(bindings_src: &str) -> Vec<Symbol> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |name: &str, kind: Kind| {
        if seen.insert(name.to_owned()) {
            out.push(Symbol {
                name: name.to_owned(),
                kind,
            });
        }
    };

    for line in bindings_src.lines() {
        // Functions sit inside `unsafe extern "C" { ... }` blocks and are
        // therefore indented; every other shape is a top-level item at column 0.
        if let Some(rest) = line.trim_start().strip_prefix("pub fn ")
            && let Some((name, after)) = leading_ident(rest)
            && name.starts_with("wlr_")
            && after.starts_with('(')
        {
            push(name, Kind::Fn);
            continue;
        }
        if let Some(rest) = line.strip_prefix("pub struct ")
            && let Some((name, after)) = leading_ident(rest)
            && name.starts_with("wlr_")
        {
            // `(pub ` is bindgen's constified-enum newtype; ` {` is a real
            // struct. Anything else (a generic, a unit struct) is not a shape
            // bindgen emits for a C type, so it is left unclassified rather
            // than guessed at.
            if after.starts_with("(pub ") {
                push(name, Kind::Enum);
            } else if after.starts_with(" {") && !name.contains("__bindgen_ty") {
                push(name, Kind::Struct);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("pub union ")
            && let Some((name, after)) = leading_ident(rest)
            && name.starts_with("wlr_")
            && after.starts_with(" {")
            && !name.contains("__bindgen_ty")
        {
            push(name, Kind::Union);
            continue;
        }
        if let Some(rest) = line.strip_prefix("pub type ")
            && let Some((name, after)) = leading_ident(rest)
            && name.starts_with("wlr_")
            && after.starts_with(" =")
        {
            push(name, Kind::TypeAlias);
        }
    }

    out.sort();
    out
}

/// Names of `wlr_*` functions bindgen bound as C variadics.
///
/// `pub fn f(a: T, ...)` is legal Rust in an `extern "C"` block, so bindgen does
/// emit these — wlroots 0.20 has one, `wlr_surface_reject_pending`. They matter
/// to the ledger because a safe wrapper cannot forward Rust arguments into a
/// `va_list`: covering one means a hand-written call site that formats first and
/// passes a single `%s`, which is a different piece of work from the 837 other
/// functions. Reporting the set is what keeps a second one from arriving inside
/// a milestone that budgeted for none.
///
/// The whole parameter list is walked, not just the declaration's first line:
/// rustfmt wraps a long signature and puts the `...` on a line of its own, which
/// is exactly the shape `wlr_surface_reject_pending` has.
pub fn variadic_fns(bindings_src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = bindings_src.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim_start().strip_prefix("pub fn ") else {
            continue;
        };
        let Some((name, mut tail)) = leading_ident(rest) else {
            continue;
        };
        if !name.starts_with("wlr_") || !tail.starts_with('(') {
            continue;
        }

        let mut depth = 0i32;
        let mut variadic = false;
        loop {
            variadic |= tail.contains("...");
            depth += tail.chars().filter(|c| *c == '(').count() as i32;
            depth -= tail.chars().filter(|c| *c == ')').count() as i32;
            // Stop at the paren that closes the parameter list; a return type
            // may carry parens of its own and says nothing about arity.
            if depth <= 0 {
                break;
            }
            let Some(next) = lines.next() else { break };
            tail = next;
        }
        if variadic {
            out.push(name.to_owned());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// One row of `coverage/wrapped.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedEntry {
    /// The `wlr_*` symbol.
    pub symbol: String,
    /// The file under `crates/wlr/src/` that reaches it, without `.rs`.
    pub module: String,
    /// The safe public item a consumer calls to get this symbol's effect.
    pub item: String,
}

/// Why a symbol is in `coverage/waived.toml` rather than `wrapped.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WaiveReason {
    /// Only reachable by implementing a wlroots vtable (M13's inverse-FFI work).
    InterfaceImplOnly,
    /// wlroots documents it as deprecated and names a replacement.
    Deprecated,
    /// Not consumer-facing: env-var knobs, private bookkeeping types.
    Internal,
    /// A different wlroots API supersedes it and is the one this crate wraps.
    SupersededBy,
    /// Belongs to a library this crate does not re-wrap (pixman, xkbcommon, …).
    ForeignLibrary,
    /// Backlog. The milestone that will wrap it is required.
    NotYet,
}

impl WaiveReason {
    /// The kebab-case spelling used in the ledger.
    pub fn as_str(self) -> &'static str {
        match self {
            WaiveReason::InterfaceImplOnly => "interface-impl-only",
            WaiveReason::Deprecated => "deprecated",
            WaiveReason::Internal => "internal",
            WaiveReason::SupersededBy => "superseded-by",
            WaiveReason::ForeignLibrary => "foreign-library",
            WaiveReason::NotYet => "not-yet",
        }
    }

    fn parse(s: &str) -> Option<WaiveReason> {
        Some(match s {
            "interface-impl-only" => WaiveReason::InterfaceImplOnly,
            "deprecated" => WaiveReason::Deprecated,
            "internal" => WaiveReason::Internal,
            "superseded-by" => WaiveReason::SupersededBy,
            "foreign-library" => WaiveReason::ForeignLibrary,
            "not-yet" => WaiveReason::NotYet,
            _ => return None,
        })
    }
}

/// One row of `coverage/waived.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaivedEntry {
    pub symbol: String,
    pub reason: WaiveReason,
    pub note: String,
    /// Required when `reason == NotYet`; optional otherwise (an
    /// `interface-impl-only` row may still name the milestone that revisits it).
    pub milestone: Option<String>,
}

/// One `[[table]]` block: the line it started on, and its `key = "value"` rows.
struct RawEntry {
    line: usize,
    fields: Vec<(String, String)>,
}

/// A deliberately strict, deliberately tiny TOML reader.
///
/// It accepts `[[<table>]]` headers, `key = "value"` lines, `#` comments and
/// blank lines — nothing else, no escapes, no multi-line values. The ledgers are
/// this crate's own format and the strictness is the point: a `toml` dependency
/// would accept syntax the ledger has no meaning for, and every accepted-but-
/// meaningless row is a row that silently covers nothing.
fn parse_tables(src: &str, table: &str) -> Result<Vec<RawEntry>, String> {
    let header = format!("[[{table}]]");
    let mut entries: Vec<RawEntry> = Vec::new();

    for (idx, raw) in src.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == header {
            entries.push(RawEntry {
                line: lineno,
                fields: Vec::new(),
            });
            continue;
        }
        if line.starts_with('[') {
            return Err(format!(
                "line {lineno}: expected `{header}`, found `{line}`"
            ));
        }

        let Some(entry) = entries.last_mut() else {
            return Err(format!(
                "line {lineno}: `{line}` appears before the first `{header}`"
            ));
        };
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "line {lineno}: expected `key = \"value\"`, found `{line}`"
            ));
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("line {lineno}: `{key}` is not a bare key"));
        }
        let value = value.trim();
        let inner = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .filter(|v| !v.contains('"'))
            .ok_or_else(|| {
                format!("line {lineno}: value for `{key}` must be a plain double-quoted string")
            })?;
        if entry.fields.iter().any(|(k, _)| k == key) {
            return Err(format!("line {lineno}: duplicate key `{key}`"));
        }
        entry.fields.push((key.to_owned(), inner.to_owned()));
    }

    Ok(entries)
}

fn field<'a>(entry: &'a RawEntry, key: &str) -> Option<&'a str> {
    entry
        .fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn require<'a>(entry: &'a RawEntry, key: &str) -> Result<&'a str, String> {
    field(entry, key).ok_or_else(|| format!("line {}: missing `{key}`", entry.line))
}

fn reject_unknown(entry: &RawEntry, allowed: &[&str]) -> Result<(), String> {
    for (k, _) in &entry.fields {
        if !allowed.contains(&k.as_str()) {
            return Err(format!(
                "line {}: unknown key `{k}` (allowed: {})",
                entry.line,
                allowed.join(", ")
            ));
        }
    }
    Ok(())
}

/// Read `coverage/wrapped.toml`.
pub fn parse_wrapped(src: &str) -> Result<Vec<WrappedEntry>, String> {
    let mut out = Vec::new();
    for entry in parse_tables(src, "wrapped")? {
        reject_unknown(&entry, &["symbol", "module", "item"])?;
        out.push(WrappedEntry {
            symbol: require(&entry, "symbol")?.to_owned(),
            module: require(&entry, "module")?.to_owned(),
            item: require(&entry, "item")?.to_owned(),
        });
    }
    Ok(out)
}

/// Read `coverage/waived.toml`.
///
/// The `milestone`-required-for-`not-yet` rule is *not* enforced here: it is a
/// ledger policy with its own gate test, and folding it into the parser would
/// make a policy failure look like a syntax error.
pub fn parse_waived(src: &str) -> Result<Vec<WaivedEntry>, String> {
    let mut out = Vec::new();
    for entry in parse_tables(src, "waived")? {
        reject_unknown(&entry, &["symbol", "reason", "note", "milestone"])?;
        let reason_str = require(&entry, "reason")?;
        let reason = WaiveReason::parse(reason_str).ok_or_else(|| {
            format!(
                "line {}: unknown reason `{reason_str}` (allowed: {})",
                entry.line,
                [
                    WaiveReason::InterfaceImplOnly,
                    WaiveReason::Deprecated,
                    WaiveReason::Internal,
                    WaiveReason::SupersededBy,
                    WaiveReason::ForeignLibrary,
                    WaiveReason::NotYet,
                ]
                .map(WaiveReason::as_str)
                .join(", ")
            )
        })?;
        out.push(WaivedEntry {
            symbol: require(&entry, "symbol")?.to_owned(),
            reason,
            note: require(&entry, "note")?.to_owned(),
            milestone: field(&entry, "milestone").map(str::to_owned),
        });
    }
    Ok(out)
}

/// This file's own basename.
///
/// It lives under `crates/wlr/src/` but is not a module of the library, and its
/// test fixtures below contain literal `sys::wlr_*` strings. Letting the scan
/// read it would mean the audit could satisfy its own staleness check from its
/// own source — `wlr_scene_create` and `wlr_box` are in those fixtures today —
/// which is precisely the silent drift the check exists to catch.
const AUDIT_FILE: &str = "coverage.rs";

/// Every `sys::wlr_*` identifier mentioned anywhere under `src_dir`.
///
/// `crates/wlr/src/sys.rs` is a single `pub(crate) use wlr_sys::*;`, so every
/// raw FFI touch in the safe crate is spelled `sys::` — which makes this scan a
/// complete, single-pattern inventory. It is the validation half of
/// `wrapped.toml`: a row whose symbol no longer appears here names a wrapper
/// that has been deleted or rewritten, and is a hard failure.
pub fn scan_sys_uses(src_dir: &Path) -> std::io::Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    let mut stack = vec![src_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs")
                && path.file_name().is_none_or(|name| name != AUDIT_FILE)
            {
                let text = fs::read_to_string(&path)?;
                collect_sys_uses(&text, &mut found);
            }
        }
    }
    Ok(found)
}

fn collect_sys_uses(text: &str, out: &mut BTreeSet<String>) {
    let mut rest = text;
    while let Some(at) = rest.find("sys::") {
        rest = &rest[at + "sys::".len()..];
        if let Some((name, _)) = leading_ident(rest)
            && name.starts_with("wlr_")
        {
            out.insert(name.to_owned());
        }
    }
}

/// The reconciliation result.
#[derive(Debug, Default)]
pub struct Report {
    /// Bound symbols in neither ledger. Hard failure.
    pub unlisted: Vec<Symbol>,
    /// `wrapped.toml` rows whose symbol no longer appears in a `sys::` use.
    /// Hard failure.
    pub stale_wrapped: Vec<String>,
    /// Symbols listed in both ledgers. Hard failure.
    pub duplicated: Vec<String>,
    /// Ledger rows naming a symbol bindgen never emitted. Hard failure in the
    /// all-features configuration only — see `coverage/README.md`.
    pub unknown_symbol: Vec<String>,
    /// The backlog. Informational; emptiness is the definition of 100%.
    pub not_yet: Vec<WaivedEntry>,
    /// `(group, wrapped, total)` for the burn-down table.
    pub per_header: Vec<(String, usize, usize)>,
}

/// Reconcile ground truth against the two ledgers.
pub fn reconcile(
    symbols: &[Symbol],
    wrapped: &[WrappedEntry],
    waived: &[WaivedEntry],
    sys_uses: &BTreeSet<String>,
) -> Report {
    let known: BTreeSet<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    let wrapped_names: BTreeSet<&str> = wrapped.iter().map(|w| w.symbol.as_str()).collect();
    let waived_names: BTreeSet<&str> = waived.iter().map(|w| w.symbol.as_str()).collect();

    let mut report = Report {
        unlisted: symbols
            .iter()
            .filter(|s| {
                !wrapped_names.contains(s.name.as_str()) && !waived_names.contains(s.name.as_str())
            })
            .cloned()
            .collect(),
        stale_wrapped: wrapped
            .iter()
            .filter(|w| !sys_uses.contains(&w.symbol))
            .map(|w| w.symbol.clone())
            .collect(),
        duplicated: wrapped_names
            .intersection(&waived_names)
            .map(|s| (*s).to_owned())
            .collect(),
        unknown_symbol: wrapped_names
            .union(&waived_names)
            .filter(|s| !known.contains(*s))
            .map(|s| (*s).to_owned())
            .collect(),
        not_yet: waived
            .iter()
            .filter(|w| w.reason == WaiveReason::NotYet)
            .cloned()
            .collect(),
        per_header: Vec::new(),
    };

    let mut groups: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    for symbol in symbols {
        let slot = groups.entry(group_of(&symbol.name)).or_default();
        slot.1 += 1;
        if wrapped_names.contains(symbol.name.as_str()) {
            slot.0 += 1;
        }
    }
    report.per_header = groups
        .into_iter()
        .map(|(g, (w, t))| (g.to_owned(), w, t))
        .collect();

    report.unlisted.sort();
    report.stale_wrapped.sort();
    report.not_yet.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    report
}

impl Report {
    /// True when no hard-failure vector has an entry.
    pub fn is_clean(&self) -> bool {
        self.unlisted.is_empty()
            && self.stale_wrapped.is_empty()
            && self.duplicated.is_empty()
            && self.unknown_symbol.is_empty()
    }

    /// True when the ledgers are clean *and* the backlog is empty — the
    /// definition of 100% coverage.
    pub fn is_complete(&self) -> bool {
        self.is_clean() && self.not_yet.is_empty()
    }

    /// Every hard failure, one symbol per line, ready to paste into a ledger.
    pub fn render_failures(&self) -> String {
        let mut s = String::new();
        if !self.unlisted.is_empty() {
            s.push_str(&format!(
                "{} symbol(s) in neither coverage/wrapped.toml nor coverage/waived.toml:\n",
                self.unlisted.len()
            ));
            for sym in &self.unlisted {
                s.push_str(&format!("  {:<6} {}\n", sym.kind.as_str(), sym.name));
            }
        }
        if !self.duplicated.is_empty() {
            s.push_str(&format!(
                "{} symbol(s) listed in both ledgers:\n",
                self.duplicated.len()
            ));
            for name in &self.duplicated {
                s.push_str(&format!("  {name}\n"));
            }
        }
        if !self.stale_wrapped.is_empty() {
            s.push_str(&format!(
                "{} wrapped.toml row(s) with no `sys::` use left in crates/wlr/src:\n",
                self.stale_wrapped.len()
            ));
            for name in &self.stale_wrapped {
                s.push_str(&format!("  {name}\n"));
            }
        }
        if !self.unknown_symbol.is_empty() {
            s.push_str(&format!(
                "{} ledger row(s) naming a symbol bindgen never emitted:\n",
                self.unknown_symbol.len()
            ));
            for name in &self.unknown_symbol {
                s.push_str(&format!("  {name}\n"));
            }
        }
        s
    }

    /// The human-facing burn-down table.
    pub fn render_table(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "{:<24} {:>7} {:>7} {:>8}\n",
            "area", "wrapped", "total", "percent"
        ));
        s.push_str(&format!("{}\n", "-".repeat(49)));
        let mut total_wrapped = 0;
        let mut total_all = 0;
        for (group, wrapped, total) in &self.per_header {
            total_wrapped += wrapped;
            total_all += total;
            s.push_str(&format!(
                "{:<24} {:>7} {:>7} {:>7.1}%\n",
                group,
                wrapped,
                total,
                percent(*wrapped, *total)
            ));
        }
        s.push_str(&format!("{}\n", "-".repeat(49)));
        s.push_str(&format!(
            "{:<24} {:>7} {:>7} {:>7.1}%\n",
            "TOTAL",
            total_wrapped,
            total_all,
            percent(total_wrapped, total_all)
        ));

        let mut by_milestone: BTreeMap<&str, usize> = BTreeMap::new();
        for entry in &self.not_yet {
            *by_milestone
                .entry(entry.milestone.as_deref().unwrap_or("(unassigned)"))
                .or_default() += 1;
        }
        s.push_str(&format!("\nnot-yet backlog ({}):\n", self.not_yet.len()));
        for (milestone, count) in by_milestone {
            s.push_str(&format!("  {milestone:<14} {count:>5}\n"));
        }
        s
    }
}

fn percent(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        100.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// Longest-prefix map from symbol name to a burn-down area.
///
/// Purely cosmetic — it shapes the report, never the gate — so an unmatched
/// symbol lands in `other` rather than failing anything. The longest matching
/// prefix wins, so rows may be added in any order and `wlr_output_transform_*`
/// still reaches `util` past the shorter `wlr_output` row.
const GROUPS: &[(&str, &str)] = &[
    ("wlr_scene", "scene"),
    ("wlr_damage_ring", "scene"),
    ("wlr_color_manager_v1", "color-management"),
    ("wlr_color_representation", "color-management"),
    ("wlr_image_description", "color-management"),
    ("wlr_color_", "render"),
    ("wlr_render", "render"),
    ("wlr_texture", "render"),
    ("wlr_swapchain", "render"),
    ("wlr_allocator", "render"),
    ("wlr_drm_format", "render"),
    ("wlr_drm_syncobj_timeline", "render"),
    ("wlr_dmabuf_attributes", "render"),
    ("wlr_egl", "render"),
    ("wlr_gles2_", "render"),
    ("wlr_vk_", "render"),
    ("wlr_pixman_", "render"),
    ("wlr_scale_filter_mode", "render"),
    ("wlr_buffer", "buffer"),
    ("wlr_drm", "buffer"),
    ("wlr_client_buffer", "buffer"),
    ("wlr_shm", "buffer"),
    ("wlr_readonly_data_buffer", "buffer"),
    ("wlr_linux_dmabuf", "buffer"),
    ("wlr_linux_drm_syncobj", "buffer"),
    ("wlr_single_pixel", "buffer"),
    ("wlr_dmabuf_v1", "buffer"),
    ("wlr_box", "util"),
    ("wlr_fbox", "util"),
    ("wlr_region_", "util"),
    ("wlr_output_transform", "util"),
    ("wlr_log", "util"),
    ("wlr_addon", "util"),
    ("wlr_edges", "util"),
    ("wlr_version_get", "util"),
    ("wlr_output_layout", "output"),
    ("wlr_output", "output"),
    ("wlr_xdg_output", "output"),
    ("wlr_gamma_control", "output"),
    ("wlr_fractional_scale", "output"),
    ("wlr_viewport", "output"),
    ("wlr_presentation", "output"),
    ("wlr_tearing_control", "output"),
    ("wlr_direction", "output"),
    ("wlr_xcursor", "pointer"),
    ("wlr_cursor", "pointer"),
    ("wlr_pointer", "pointer"),
    ("wlr_relative_pointer", "pointer"),
    ("wlr_touch", "pointer"),
    ("wlr_switch", "pointer"),
    ("wlr_seat_pointer", "pointer"),
    ("wlr_seat_touch", "pointer"),
    ("wlr_button_state", "pointer"),
    ("wlr_keyboard", "keyboard"),
    ("wlr_seat_keyboard", "keyboard"),
    ("wlr_virtual_keyboard", "keyboard"),
    ("wlr_virtual_pointer", "keyboard"),
    ("wlr_transient_seat", "keyboard"),
    ("wlr_tablet", "tablet"),
    ("wlr_send_tablet", "tablet"),
    ("wlr_input_method", "text-input"),
    ("wlr_input_popup", "text-input"),
    ("wlr_text_input", "text-input"),
    ("wlr_seat", "seat"),
    ("wlr_serial_ring", "seat"),
    ("wlr_serial_range", "seat"),
    ("wlr_xdg_", "shell"),
    ("wlr_layer_", "shell"),
    ("wlr_foreign_toplevel", "shell"),
    ("wlr_ext_foreign_toplevel", "shell"),
    ("wlr_ext_workspace", "shell"),
    ("wlr_session_lock", "shell"),
    ("wlr_security_context", "shell"),
    ("wlr_compositor", "shell"),
    ("wlr_subcompositor", "shell"),
    ("wlr_subsurface", "shell"),
    ("wlr_surface", "shell"),
    ("wlr_fixes", "shell"),
    ("wlr_screencopy", "capture"),
    ("wlr_export_dmabuf", "capture"),
    ("wlr_ext_image", "capture"),
    ("wlr_ext_output_image_capture", "capture"),
    ("wlr_drm_lease", "capture"),
    ("wlr_data_control", "selection"),
    ("wlr_ext_data_control", "selection"),
    ("wlr_primary_selection", "selection"),
    ("wlr_data_device", "selection"),
    ("wlr_data_offer", "selection"),
    ("wlr_data_source", "selection"),
    ("wlr_drag", "selection"),
    ("wlr_idle_", "misc"),
    ("wlr_alpha_mod", "misc"),
    ("wlr_content_type", "misc"),
    ("wlr_server_decoration", "misc"),
    ("wlr_cursor_shape", "misc"),
    ("wlr_keyboard_shortcuts", "misc"),
    ("wlr_pointer_constraint", "misc"),
    ("wlr_pointer_gestures", "misc"),
    ("wlr_xwayland", "xwayland"),
    ("wlr_xwm", "xwayland"),
    ("wlr_backend", "backend"),
    ("wlr_multi_", "backend"),
    ("wlr_headless_", "backend"),
    ("wlr_x11_", "backend"),
    ("wlr_wl_", "backend"),
    ("wlr_libinput_", "backend"),
    ("wlr_drm_", "backend"),
    ("wlr_session", "backend"),
    ("wlr_device", "backend"),
    ("wlr_input_device", "backend"),
];

/// The burn-down area a symbol belongs to.
pub fn group_of(symbol: &str) -> &'static str {
    let mut best: Option<(&str, &'static str)> = None;
    for (prefix, group) in GROUPS {
        if symbol.starts_with(prefix)
            && best.is_none_or(|(current, _)| prefix.len() > current.len())
        {
            best = Some((prefix, group));
        }
    }
    best.map_or("other", |(_, group)| group)
}

/// Walk up from `start` to the directory holding the workspace `Cargo.toml`.
pub fn workspace_root(start: &Path) -> Result<PathBuf, String> {
    for dir in start.ancestors() {
        let manifest = dir.join("Cargo.toml");
        if let Ok(text) = fs::read_to_string(&manifest)
            && text.contains("[workspace]")
        {
            return Ok(dir.to_path_buf());
        }
    }
    Err(format!(
        "no Cargo.toml with a [workspace] table above {}",
        start.display()
    ))
}

/// Locate the `bindings.rs` to audit against.
///
/// `WLR_COVERAGE_BINDINGS` wins when set, so a caller that just ran a specific
/// feature configuration can point at exactly its output. Otherwise the newest
/// `target/*/build/wlr-sys-*/out/bindings.rs` is used.
///
/// mtime is compared only *between* candidate `bindings.rs` files, never against
/// a source file: every file in a freshly created git worktree carries the
/// checkout timestamp, so "build.rs is newer than bindings.rs" is a false alarm
/// there and would fail the gate on a clean checkout. Freshness is CI's job —
/// it builds `wlr-sys --all-features` immediately before running this.
pub fn find_bindings(workspace_root: &Path) -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("WLR_COVERAGE_BINDINGS") {
        let path = PathBuf::from(explicit);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "WLR_COVERAGE_BINDINGS points at {}, which is not a file",
                path.display()
            ))
        };
    }

    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));

    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let profiles =
        fs::read_dir(&target).map_err(|err| format!("cannot read {}: {err}", target.display()))?;
    for profile in profiles.flatten() {
        let build = profile.path().join("build");
        let Ok(dirs) = fs::read_dir(&build) else {
            continue;
        };
        for dir in dirs.flatten() {
            let name = dir.file_name();
            if !name.to_string_lossy().starts_with("wlr-sys-") {
                continue;
            }
            let candidate = dir.path().join("out/bindings.rs");
            let Ok(meta) = fs::metadata(&candidate) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            if best.as_ref().is_none_or(|(best_at, _)| mtime > *best_at) {
                best = Some((mtime, candidate));
            }
        }
    }

    best.map(|(_, path)| path).ok_or_else(|| {
        format!(
            "no wlr-sys bindings.rs under {}; run `cargo build -p wlr-sys --all-features` first",
            target.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature stand-in for bindgen's output, carrying one instance of
    /// every shape the extractor must classify and every shape it must ignore.
    const FIXTURE: &str = r#"
/* automatically generated by rust-bindgen */
pub const WLR_VERSION_MAJOR: u32 = 0;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct wlr_box {
    pub x: ::std::os::raw::c_int,
}
#[repr(transparent)]
pub struct wlr_log_importance(pub ::std::os::raw::c_uint);
pub type wlr_log_func_t = ::std::option::Option<unsafe extern "C" fn()>;
#[repr(C)]
pub struct wlr_xdg_surface__bindgen_ty_1 {
    pub toplevel: *mut wlr_xdg_toplevel,
}
#[repr(C)]
pub union wlr_scene_node__bindgen_ty_1 {
    pub tree: *mut wlr_scene_tree,
}
#[repr(C)]
pub union wlr_real_union {
    pub a: u32,
}
#[repr(C)]
pub union wlr_scene_payload {
    pub tree: *mut wlr_scene_tree,
}
const _: () = {
    ["Size of wlr_box"][::std::mem::size_of::<wlr_box>() - 16usize];
};
unsafe extern "C" {
    pub fn wlr_box_empty(box_: *const wlr_box) -> bool;
}
unsafe extern "C" {
    pub fn wlr_box_intersection(
        dest: *mut wlr_box,
        box_a: *const wlr_box,
        box_b: *const wlr_box,
    ) -> bool;
}
unsafe extern "C" {
    pub fn wlr_log_vararg(fmt: *const ::std::os::raw::c_char, ...);
}
unsafe extern "C" {
    pub fn wlr_surface_reject_pending(
        surface: *mut wlr_surface,
        resource: *mut wl_resource,
        code: u32,
        msg: *const ::std::os::raw::c_char,
        ...
    );
}
unsafe extern "C" {
    pub fn pixman_region32_init(region: *mut pixman_region32);
}
"#;

    #[test]
    fn extract_classifies_every_bindgen_shape() {
        let symbols = extract(FIXTURE);
        let names: Vec<(&str, Kind)> = symbols.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert_eq!(
            names,
            vec![
                ("wlr_box", Kind::Struct),
                ("wlr_box_empty", Kind::Fn),
                ("wlr_box_intersection", Kind::Fn),
                ("wlr_log_func_t", Kind::TypeAlias),
                ("wlr_log_importance", Kind::Enum),
                ("wlr_log_vararg", Kind::Fn),
                ("wlr_real_union", Kind::Union),
                ("wlr_scene_payload", Kind::Union),
                ("wlr_surface_reject_pending", Kind::Fn),
            ]
        );
    }

    #[test]
    fn extract_excludes_anonymous_aggregates_layout_asserts_and_foreign_fns() {
        let names: BTreeSet<String> = extract(FIXTURE).into_iter().map(|s| s.name).collect();
        assert!(!names.iter().any(|n| n.contains("__bindgen_ty")));
        assert!(!names.contains("pixman_region32_init"));
        // The layout-assertion block names `wlr_box` inside a `const _` — the
        // `pub ` anchor is what keeps it from being counted a second time.
        assert_eq!(names.iter().filter(|n| *n == "wlr_box").count(), 1);
    }

    #[test]
    fn extract_handles_a_multi_line_parameter_list() {
        let symbols = extract(FIXTURE);
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "wlr_box_intersection" && s.kind == Kind::Fn)
        );
    }

    /// Both spellings must be caught. `wlr_surface_reject_pending` is the real
    /// one wlroots 0.20 ships, and rustfmt wraps its signature so the `...` is
    /// on a line by itself — a first-line-only scan reports zero variadics on a
    /// tree that has one, which is a false green in the gate that reads this.
    #[test]
    fn variadic_functions_are_flagged_on_one_line_and_across_many() {
        assert_eq!(
            variadic_fns(FIXTURE),
            vec![
                "wlr_log_vararg".to_owned(),
                "wlr_surface_reject_pending".to_owned()
            ]
        );
    }

    /// A non-variadic wrapped signature must not be swept in by the walk, and
    /// the walk must stop at the parameter list rather than running to EOF.
    #[test]
    fn a_wrapped_non_variadic_signature_is_not_flagged() {
        assert!(!variadic_fns(FIXTURE).contains(&"wlr_box_intersection".to_owned()));
    }

    #[test]
    fn wrapped_ledger_round_trips() {
        let src = "\
# a comment
[[wrapped]]
symbol = \"wlr_backend_autocreate\"
module = \"backend\"
item   = \"Backend::autocreate\"

[[wrapped]]
symbol = \"wlr_output\"
module = \"output\"
item   = \"Output\"
";
        let parsed = parse_wrapped(src).expect("parses");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].symbol, "wlr_backend_autocreate");
        assert_eq!(parsed[1].item, "Output");
    }

    #[test]
    fn waived_ledger_round_trips() {
        let src = "\
[[waived]]
symbol    = \"wlr_renderer_init\"
reason    = \"interface-impl-only\"
note      = \"Rust implementing a wlroots vtable.\"
milestone = \"M13\"

[[waived]]
symbol = \"wlr_scene_tree_create\"
reason = \"not-yet\"
note   = \"SceneTree lands in M5.\"
milestone = \"M5\"
";
        let parsed = parse_waived(src).expect("parses");
        assert_eq!(parsed[0].reason, WaiveReason::InterfaceImplOnly);
        assert_eq!(parsed[1].reason, WaiveReason::NotYet);
        assert_eq!(parsed[1].milestone.as_deref(), Some("M5"));
    }

    #[test]
    fn malformed_ledgers_name_the_line() {
        let missing_table = "symbol = \"wlr_x\"\n";
        assert_eq!(
            parse_wrapped(missing_table).unwrap_err(),
            "line 1: `symbol = \"wlr_x\"` appears before the first `[[wrapped]]`"
        );

        let wrong_table = "[[waived]]\nsymbol = \"wlr_x\"\n";
        assert_eq!(
            parse_wrapped(wrong_table).unwrap_err(),
            "line 1: expected `[[wrapped]]`, found `[[waived]]`"
        );

        let unquoted = "[[wrapped]]\nsymbol = wlr_x\n";
        assert_eq!(
            parse_wrapped(unquoted).unwrap_err(),
            "line 2: value for `symbol` must be a plain double-quoted string"
        );

        let unknown_key = "[[wrapped]]\nsymbol = \"wlr_x\"\nwhere = \"here\"\n";
        assert_eq!(
            parse_wrapped(unknown_key).unwrap_err(),
            "line 1: unknown key `where` (allowed: symbol, module, item)"
        );

        let duplicate = "[[wrapped]]\nsymbol = \"wlr_x\"\nsymbol = \"wlr_y\"\n";
        assert_eq!(
            parse_wrapped(duplicate).unwrap_err(),
            "line 3: duplicate key `symbol`"
        );

        let incomplete = "[[wrapped]]\nsymbol = \"wlr_x\"\nmodule = \"m\"\n";
        assert_eq!(
            parse_wrapped(incomplete).unwrap_err(),
            "line 1: missing `item`"
        );

        let bad_reason = "[[waived]]\nsymbol = \"wlr_x\"\nreason = \"because\"\nnote = \"n\"\n";
        assert!(
            parse_waived(bad_reason)
                .unwrap_err()
                .starts_with("line 1: unknown reason `because`")
        );
    }

    #[test]
    fn sys_uses_are_collected_and_filtered() {
        let mut found = BTreeSet::new();
        collect_sys_uses(
            "let p = unsafe { sys::wlr_scene_create() };\n\
             let l: *mut sys::wl_listener = std::ptr::null_mut();\n\
             use crate::sys::wlr_box;\n",
            &mut found,
        );
        assert!(found.contains("wlr_scene_create"));
        assert!(found.contains("wlr_box"));
        assert!(!found.contains("wl_listener"));
    }

    /// The staleness check is only worth anything if the audit cannot answer it
    /// out of its own source. `coverage.rs` carries `sys::wlr_*` strings in the
    /// fixtures above, so it is skipped.
    #[test]
    fn the_scan_skips_the_audit_file_itself() {
        let dir = std::env::temp_dir().join(format!(
            "wlr-coverage-scan-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let nested = dir.join("render");
        fs::create_dir_all(&nested).expect("temp dir");
        fs::write(dir.join("runtime.rs"), "sys::wlr_from_a_real_module()").expect("write");
        fs::write(nested.join("pass.rs"), "sys::wlr_from_a_nested_module()").expect("write");
        fs::write(dir.join(AUDIT_FILE), "sys::wlr_from_the_audit_fixture()").expect("write");
        fs::write(dir.join("notes.txt"), "sys::wlr_from_a_non_rust_file()").expect("write");

        let found = scan_sys_uses(&dir).expect("scan");
        fs::remove_dir_all(&dir).expect("cleanup");

        assert!(found.contains("wlr_from_a_real_module"));
        assert!(found.contains("wlr_from_a_nested_module"));
        assert!(!found.contains("wlr_from_the_audit_fixture"));
        assert!(!found.contains("wlr_from_a_non_rust_file"));
    }

    fn sym(name: &str, kind: Kind) -> Symbol {
        Symbol {
            name: name.to_owned(),
            kind,
        }
    }

    fn wrapped(symbol: &str) -> WrappedEntry {
        WrappedEntry {
            symbol: symbol.to_owned(),
            module: "runtime".to_owned(),
            item: "Runtime::thing".to_owned(),
        }
    }

    fn waived(symbol: &str, reason: WaiveReason) -> WaivedEntry {
        WaivedEntry {
            symbol: symbol.to_owned(),
            reason,
            note: "n".to_owned(),
            milestone: (reason == WaiveReason::NotYet).then(|| "M6".to_owned()),
        }
    }

    fn uses(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn reconcile_accepts_a_consistent_ledger() {
        let symbols = vec![sym("wlr_a", Kind::Fn), sym("wlr_b", Kind::Struct)];
        let report = reconcile(
            &symbols,
            &[wrapped("wlr_a")],
            &[waived("wlr_b", WaiveReason::NotYet)],
            &uses(&["wlr_a"]),
        );
        assert!(report.is_clean());
        assert!(!report.is_complete(), "one not-yet row is still a backlog");
        assert_eq!(report.not_yet.len(), 1);
    }

    #[test]
    fn reconcile_reports_an_unlisted_symbol() {
        let symbols = vec![sym("wlr_a", Kind::Fn), sym("wlr_b", Kind::Enum)];
        let report = reconcile(&symbols, &[wrapped("wlr_a")], &[], &uses(&["wlr_a"]));
        assert!(!report.is_clean());
        assert_eq!(report.unlisted, vec![sym("wlr_b", Kind::Enum)]);
        let failures = report.render_failures();
        assert!(failures.contains("enum"), "{failures}");
        assert!(failures.contains("wlr_b"), "{failures}");
    }

    #[test]
    fn reconcile_reports_a_stale_wrapped_row() {
        let symbols = vec![sym("wlr_a", Kind::Fn)];
        let report = reconcile(&symbols, &[wrapped("wlr_a")], &[], &uses(&[]));
        assert!(!report.is_clean());
        assert_eq!(report.stale_wrapped, vec!["wlr_a".to_owned()]);
    }

    #[test]
    fn reconcile_reports_a_symbol_in_both_ledgers() {
        let symbols = vec![sym("wlr_a", Kind::Fn)];
        let report = reconcile(
            &symbols,
            &[wrapped("wlr_a")],
            &[waived("wlr_a", WaiveReason::Internal)],
            &uses(&["wlr_a"]),
        );
        assert!(!report.is_clean());
        assert_eq!(report.duplicated, vec!["wlr_a".to_owned()]);
    }

    #[test]
    fn reconcile_reports_a_ledger_row_bindgen_never_emitted() {
        let symbols = vec![sym("wlr_a", Kind::Fn)];
        let report = reconcile(
            &symbols,
            &[wrapped("wlr_a")],
            &[waived("wlr_gone", WaiveReason::Internal)],
            &uses(&["wlr_a"]),
        );
        assert!(!report.is_clean());
        assert_eq!(report.unknown_symbol, vec!["wlr_gone".to_owned()]);
    }

    #[test]
    fn complete_means_clean_with_an_empty_backlog() {
        let symbols = vec![sym("wlr_a", Kind::Fn), sym("wlr_b", Kind::Struct)];
        let report = reconcile(
            &symbols,
            &[wrapped("wlr_a")],
            &[waived("wlr_b", WaiveReason::Internal)],
            &uses(&["wlr_a"]),
        );
        assert!(report.is_complete());
    }

    #[test]
    fn groups_use_the_longest_matching_prefix() {
        assert_eq!(group_of("wlr_scene_node_set_position"), "scene");
        assert_eq!(group_of("wlr_output_transform_invert"), "util");
        assert_eq!(group_of("wlr_output_layout_add"), "output");
        assert_eq!(group_of("wlr_color_transform_init_srgb"), "render");
        assert_eq!(group_of("wlr_color_manager_v1_create"), "color-management");
        assert_eq!(group_of("wlr_totally_new_thing"), "other");
    }
}
