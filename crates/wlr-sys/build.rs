//! Build script for `wlr-sys`.
//!
//! Locates wlroots 0.20 via pkg-config, reconciles the crate's Cargo features
//! against the subsystems the installed library was actually compiled with,
//! generates the two `wlr-protocols` server headers that wlroots' public headers
//! `#include` but do not ship, and runs bindgen over the result.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The wlroots minor series this crate binds.
///
/// Bumping to a new wlroots minor touches more than this constant — see
/// `docs/RELEASING.md` for the full checklist. Everything in *this file* is
/// derived from the two constants below, deliberately: an earlier revision
/// hardcoded the upper bound as a bare `"0.21"`, so the obvious
/// `sed 's/0.20/0.21/'` produced `range_version("0.21".."0.21")` — an empty
/// range that rejects every installed wlroots, with no occurrence of the
/// searched string to reveal it.
const WLROOTS_MINOR: &str = "0.20";
const WLROOTS_NEXT_MINOR: &str = "0.21";

/// The pkg-config module name. wlroots embeds its minor version here because it
/// has no stable ABI: `libwlroots-0.20.so` and `libwlroots-0.21.so` coexist.
const WLROOTS_PC: &str = concat!("wlroots-", "0.20");

/// Cfg names referenced outside the `SUBSYSTEMS` table. Hoisted so a rename in
/// the table is a compile error here rather than a silently-stopped match — the
/// two consumers below drive transitive header inclusion and the libinput
/// blocklist, and both fail silently when a string comparison stops matching.
const CFG_DRM_BACKEND: &str = "wlr_has_drm_backend";
const CFG_LIBINPUT_BACKEND: &str = "wlr_has_libinput_backend";

/// Protocol XMLs vendored under `protocol/`, from the `wlr-protocols` repository.
///
/// wlroots' installed headers `#include "wlr-layer-shell-unstable-v1-protocol.h"`
/// and `#include "wlr-output-power-management-unstable-v1-protocol.h"`, but those
/// generated headers are private to the wlroots build and are never installed.
/// We regenerate them with `wayland-scanner` into `OUT_DIR`.
const PROTOCOLS: &[&str] = &[
    "wlr-layer-shell-unstable-v1",
    "wlr-output-power-management-unstable-v1",
];

/// A wlroots compile-time subsystem.
struct Subsystem {
    /// Cargo feature that requests it, or `None` for detect-only subsystems that
    /// gate no public header and are surfaced purely as a `cfg`.
    feature: Option<&'static str>,
    /// The `have_*` variable in `wlroots-0.20.pc`.
    pc_var: &'static str,
    /// The `cfg` emitted when the subsystem is both requested and available.
    cfg: &'static str,
    /// Headers bound only when this subsystem is enabled.
    headers: &'static [&'static str],
    /// Additional pkg-config modules whose include paths those headers need.
    extra_pc: &'static [&'static str],
}

const SUBSYSTEMS: &[Subsystem] = &[
    Subsystem {
        feature: Some("drm-backend"),
        pc_var: "have_drm_backend",
        cfg: CFG_DRM_BACKEND,
        headers: &["wlr/backend/drm.h"],
        extra_pc: &[],
    },
    Subsystem {
        feature: Some("x11-backend"),
        pc_var: "have_x11_backend",
        cfg: "wlr_has_x11_backend",
        headers: &["wlr/backend/x11.h"],
        extra_pc: &[],
    },
    Subsystem {
        feature: Some("libinput-backend"),
        pc_var: "have_libinput_backend",
        cfg: CFG_LIBINPUT_BACKEND,
        headers: &["wlr/backend/libinput.h"],
        extra_pc: &["libinput"],
    },
    Subsystem {
        feature: Some("session"),
        pc_var: "have_session",
        cfg: "wlr_has_session",
        headers: &["wlr/backend/session.h"],
        extra_pc: &[],
    },
    Subsystem {
        feature: Some("gles2-renderer"),
        pc_var: "have_gles2_renderer",
        cfg: "wlr_has_gles2_renderer",
        headers: &["wlr/render/gles2.h"],
        extra_pc: &["glesv2"],
    },
    Subsystem {
        feature: Some("vulkan-renderer"),
        pc_var: "have_vulkan_renderer",
        cfg: "wlr_has_vulkan_renderer",
        headers: &["wlr/render/vulkan.h"],
        extra_pc: &["vulkan"],
    },
    Subsystem {
        feature: Some("xwayland"),
        pc_var: "have_xwayland",
        cfg: "wlr_has_xwayland",
        headers: &[
            "wlr/xwayland.h",
            "wlr/xwayland/server.h",
            "wlr/xwayland/shell.h",
            "wlr/xwayland/xwayland.h",
        ],
        extra_pc: &["xcb"],
    },
    // Detect-only: these gate no public header, so they get no Cargo feature.
    // `wlr/render/color.h` is always bindable — `have_color_management` only
    // reports whether ICC support was compiled in. Giving it a feature would
    // have made the cfg suppressible on a machine where the capability is
    // genuinely present, i.e. a knob whose only reachable states are "correct"
    // and "lying".
    Subsystem {
        feature: None,
        pc_var: "have_color_management",
        cfg: "wlr_has_color_management",
        headers: &[],
        extra_pc: &[],
    },
    Subsystem {
        feature: None,
        pc_var: "have_gbm_allocator",
        cfg: "wlr_has_gbm_allocator",
        headers: &[],
        extra_pc: &[],
    },
    Subsystem {
        feature: None,
        pc_var: "have_udmabuf_allocator",
        cfg: "wlr_has_udmabuf_allocator",
        headers: &[],
        extra_pc: &[],
    },
];

/// Types blocklisted in bindgen and re-imported from `wayland-sys`, so that
/// `wl_display`, `wl_listener` and friends are the *same* Rust types here as in
/// the rest of the wayland-rs ecosystem.
///
/// Deliberately an explicit list rather than a `wl_.*` regex: the core protocol
/// enums that wlroots' headers reach (`wl_output_transform`) are *not* provided
/// by `wayland-sys` and must be generated locally.
///
/// Keeping this list complete is not optional, and a missing entry fails
/// silently: bindgen generates its own definition, the glob import loses to it,
/// and everything still compiles while downstream gets the wrong type. That is
/// how `libinput_tablet_tool` escaped. `tests/interop.rs` now asserts type
/// identity against the re-exported crates, so a re-shadowing breaks the build.
///
/// Do not trim this list by grepping the `wlr/` headers for each name. Several
/// entries are reachable only transitively, through `wayland-server-core.h` and
/// `wayland-util.h`. Verified: removing `wl_interface`, `wl_message`,
/// `wl_notify_func_t` or `wl_resource_destroy_func_t` reintroduces a duplicate.
const WAYLAND_COMMON_TYPES: &[&str] = &[
    "wl_list",
    "wl_array",
    "wl_interface",
    "wl_message",
    "wl_argument",
    "wl_fixed_t",
    "wl_dispatcher_func_t",
];

const WAYLAND_SERVER_TYPES: &[&str] = &[
    "wl_client",
    "wl_display",
    "wl_event_loop",
    "wl_event_source",
    "wl_global",
    "wl_resource",
    "wl_shm_buffer",
    "wl_listener",
    "wl_signal",
    "wl_notify_func_t",
    "wl_resource_destroy_func_t",
    "wl_global_bind_func_t",
    "wl_display_global_filter_func_t",
    "wl_event_loop_fd_func_t",
    "wl_event_loop_timer_func_t",
    "wl_event_loop_signal_func_t",
    "wl_event_loop_idle_func_t",
    "wl_client_for_each_resource_iterator_func_t",
];

// `drmModeModeInfo` is deliberately *not* blocklisted. wlroots only
// forward-declares it and passes it by pointer, so bindgen binds it opaque and
// no layout is at stake. `drm-sys` would not help anyway: it generates the
// kernel uAPI header and exposes the distinct `drm_mode_modeinfo`, whereas
// wlroots means libdrm's userspace struct from <xf86drmMode.h>.

const XKB_TYPES: &[&str] = &[
    "xkb_context",
    "xkb_keymap",
    "xkb_state",
    "xkb_rule_names",
    "xkb_keysym_t",
    "xkb_mod_mask_t",
    "xkb_led_mask_t",
    "xkb_led_index_t",
    "xkb_layout_index_t",
    "xkb_mod_index_t",
];

const INPUT_TYPES: &[&str] = &[
    "libinput",
    "libinput_device",
    "libinput_device_group",
    "libinput_seat",
    "libinput_event",
    "libinput_tablet_tool",
];

fn main() {
    // `pkg-config` emits its own rerun-if-env-changed for PKG_CONFIG_PATH,
    // PKG_CONFIG_LIBDIR, PKG_CONFIG_SYSROOT_DIR and the target-suffixed variants.
    println!("cargo::rerun-if-changed=build.rs");
    for cfg in SUBSYSTEMS.iter().map(|s| s.cfg) {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR unset"));

    // docs.rs has no wlroots installation and will not install one, so a
    // build-time-bindgen crate cannot document itself there. Fall back to a
    // committed snapshot so the API is at least browsable; CI regenerates it and
    // fails if it has drifted, which is what keeps the snapshot honest.
    println!("cargo::rerun-if-env-changed=DOCS_RS");
    if env::var_os("DOCS_RS").is_some() {
        docs_rs_fallback(&out_dir);
        return;
    }

    let wlroots = probe_wlroots();
    let include_dir = wlroots_include_dir(&wlroots);

    // `src/signal.rs` declares `wl_signal_emit_mutable` directly, so this crate
    // needs libwayland-server on its own link line. Relying on `wayland-sys`'s
    // `#[link]` would break the moment anything in the graph enables its
    // `dlopen` feature, which replaces that attribute with runtime loading.
    pkg_config::Config::new()
        .probe("wayland-server")
        .expect("wlr-sys requires wayland-server; it is a hard dependency of wlroots itself");

    for proto in PROTOCOLS {
        println!("cargo::rerun-if-changed=protocol/{proto}.xml");
    }
    let protocol_dir = generate_protocol_headers(&out_dir);

    let mut clang_args: Vec<String> = vec![
        "-DWLR_USE_UNSTABLE".to_owned(),
        format!("-I{}", protocol_dir.display()),
    ];
    clang_args.extend(include_flags(&wlroots.include_paths));
    // wlroots' own Cflags do not mention EGL, but `wlr/render/egl.h` is
    // unconditional and includes <EGL/egl.h>.
    clang_args.extend(probe_include_paths("egl", None));

    let mut enabled = reconcile_subsystems();
    let enabled_cfgs = std::mem::take(&mut enabled.cfgs);
    let mut gated_headers = std::mem::take(&mut enabled.headers);
    clang_args.extend(std::mem::take(&mut enabled.includes));

    // `wlr/backend/drm.h` and `wlr/backend/libinput.h` both include
    // `wlr/backend/session.h`. The `drm-backend` and `libinput-backend` Cargo
    // features therefore imply `session` (see Cargo.toml), so reaching here with
    // either enabled guarantees the session subsystem was reconciled too — the
    // cfg and the bound symbols cannot disagree.
    if enabled_cfgs.contains(&CFG_DRM_BACKEND) || enabled_cfgs.contains(&CFG_LIBINPUT_BACKEND) {
        gated_headers.insert("wlr/backend/session.h");
    }

    emit_subsystem_metadata(&enabled_cfgs);

    // Several probes report overlapping include paths; hand bindgen each once.
    // `retain` keeps the first occurrence, and clang resolves `-I` first-match
    // first, so dropping later duplicates cannot change resolution.
    let mut seen = BTreeSet::new();
    clang_args.retain(|arg| seen.insert(arg.clone()));

    let headers = collect_headers(&include_dir, &gated_headers);
    let wrapper = write_wrapper(&out_dir, &headers);
    println!("cargo::rerun-if-changed={}", include_dir.display());

    let bind_libinput = enabled_cfgs.contains(&CFG_LIBINPUT_BACKEND);
    generate_bindings(&wrapper, &clang_args, &out_dir, bind_libinput);
}

/// Documentation-only build: use the committed bindings snapshot.
///
/// Nothing here links, so no `rustc-link-lib` is emitted and no pkg-config runs.
/// Every `wlr_has_*` cfg is asserted so the rendered docs show the full API
/// surface rather than the subset one machine happened to have.
fn docs_rs_fallback(out_dir: &Path) {
    let snapshot =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset"))
            .join("prebuilt/bindings-docsrs.rs");
    println!("cargo::rerun-if-changed={}", snapshot.display());
    fs::copy(&snapshot, out_dir.join("bindings.rs")).unwrap_or_else(|err| {
        panic!(
            "DOCS_RS is set but the committed bindings snapshot at {} could not be read: {err}. \
             Regenerate it with `cargo xtask docsrs-snapshot` (see docs/RELEASING.md).",
            snapshot.display()
        )
    });
    for sub in SUBSYSTEMS {
        println!("cargo::rustc-cfg={}", sub.cfg);
        println!("cargo::metadata={}=true", sub.pc_var);
    }
}

/// What the feature/`have_*` reconciliation decided.
struct Enabled {
    cfgs: Vec<&'static str>,
    headers: BTreeSet<&'static str>,
    includes: Vec<String>,
}

/// Reconcile each subsystem's Cargo feature against what the installed library
/// actually has, per the "Reconcile features against reality" step in the design
/// spec: a subsystem is enabled iff its feature is on *and* its `have_*` is
/// `true`.
fn reconcile_subsystems() -> Enabled {
    let mut enabled = Enabled {
        cfgs: Vec::new(),
        headers: BTreeSet::new(),
        includes: Vec::new(),
    };

    for sub in SUBSYSTEMS {
        // Detect-only subsystems have no feature and are governed purely by the
        // installed library. Checking this first also avoids spawning
        // `pkg-config` for a subsystem the consumer already turned off.
        if !sub.feature.is_none_or(feature_enabled) {
            continue;
        }

        // wlroots writes `have_*` as the literal strings `true` / `false`.
        if have_flag(sub.pc_var) {
            enabled.cfgs.push(sub.cfg);
            enabled.headers.extend(sub.headers.iter().copied());
            for pc in sub.extra_pc {
                enabled
                    .includes
                    .extend(probe_include_paths(pc, sub.feature));
            }
        } else if let Some(feature) = sub.feature {
            // Deliberately a warning, not an error, so a distro that rebuilt
            // wlroots without a subsystem degrades rather than breaks. Note that
            // cargo hides build-script warnings for registry dependencies, so a
            // downstream consumer will not see this — which is exactly why the
            // decision is also published as `DEP_WLROOTS_*` metadata below.
            println!(
                "cargo::warning=feature `{feature}` is enabled, but the installed \
                 {WLROOTS_PC} does not report {}=true; disabling it. Rebuild wlroots with \
                 that subsystem, or turn the feature off to silence this warning. \
                 (Run with `cargo build -vv` if you are not seeing this from a dependency.)",
                sub.pc_var
            );
        }
    }
    enabled
}

/// Publish the reconciliation result to *dependent* crates.
///
/// `cargo::rustc-cfg` reaches only this package's own targets — it is **not**
/// propagated to dependents, so a downstream `#[cfg(wlr_has_xwayland)]` would
/// silently evaluate false and compile the guarded code away. `cargo::metadata`
/// is the channel that does cross the boundary: because this package sets
/// `links = "wlroots"`, each key below arrives in a dependent's build script as
/// `DEP_WLROOTS_<KEY>`. See the "Feature detection" section of README.md for the
/// four-line `build.rs` a consumer needs.
fn emit_subsystem_metadata(enabled_cfgs: &[&str]) {
    for cfg in enabled_cfgs {
        println!("cargo::rustc-cfg={cfg}");
    }
    for sub in SUBSYSTEMS {
        let on = enabled_cfgs.contains(&sub.cfg);
        println!("cargo::metadata={}={}", sub.pc_var, on);
    }
}

/// Locate wlroots 0.20, with actionable errors for the two common failures.
fn probe_wlroots() -> pkg_config::Library {
    match pkg_config::Config::new()
        .range_version(WLROOTS_MINOR..WLROOTS_NEXT_MINOR)
        .probe(WLROOTS_PC)
    {
        Ok(lib) => lib,
        Err(pkg_config::Error::CrossCompilation) => panic!(
            "cannot probe {WLROOTS_PC} while cross-compiling from {} to {}.\n\
             pkg-config refuses to run by default for a foreign target, because the host's \
             `.pc` files describe the wrong library.\n\
             Point it at your target sysroot with PKG_CONFIG_PATH_{} (or PKG_CONFIG_SYSROOT_DIR), \
             or set PKG_CONFIG_ALLOW_CROSS=1 if you are certain the host `.pc` files are correct.",
            env::var("HOST").unwrap_or_else(|_| "?".into()),
            env::var("TARGET").unwrap_or_else(|_| "?".into()),
            env::var("TARGET").unwrap_or_else(|_| "<target>".into()),
        ),
        Err(err) => {
            let installed = pkg_config::Config::new()
                .cargo_metadata(false)
                .probe(WLROOTS_PC)
                .ok()
                .map(|lib| lib.version);
            match installed {
                Some(version) => {
                    // wlroots has no stable ABI, so wlr-sys pins one minor per
                    // release. Name the one that matches what is installed —
                    // but only when the version actually parses, so a malformed
                    // `Version:` field cannot produce "install `wlr-sys
                    // unknown.x`".
                    let mut parts = version.split('.');
                    let matching = match (parts.next(), parts.next()) {
                        (Some(major), Some(minor))
                            if major.parse::<u32>().is_ok()
                                && minor
                                    .split('-')
                                    .next()
                                    .is_some_and(|m| m.parse::<u32>().is_ok()) =>
                        {
                            let minor = minor.split('-').next().unwrap_or(minor);
                            format!(
                                "the matching release is `wlr-sys {major}.{minor}.x`, if one has \
                                 been published"
                            )
                        }
                        _ => "use the wlr-sys release whose minor matches your wlroots".to_owned(),
                    };
                    panic!(
                        "wlr-sys {} binds wlroots {WLROOTS_MINOR}.x, but {WLROOTS_PC} reports \
                         version {version}. wlroots has no stable ABI, so each minor gets its \
                         own wlr-sys minor: {matching}. Alternatively install \
                         wlroots {WLROOTS_MINOR}.\n\n\
                         pkg-config said: {err}",
                        env!("CARGO_PKG_VERSION"),
                    )
                }
                None => panic!(
                    "could not find `{WLROOTS_PC}` via pkg-config.\n\
                     Install wlroots {WLROOTS_MINOR} and its development headers:\n  \
                     Arch:   pacman -S wlroots0.20\n  \
                     Fedora: dnf install wlroots-devel\n  \
                     Debian: apt install libwlroots-0.20-dev\n\
                     If it is installed somewhere unusual, set PKG_CONFIG_PATH.\n\n\
                     pkg-config said: {err}"
                ),
            }
        }
    }
}

/// The directory containing `wlr/`, i.e. `<includedir>/wlroots-0.20`.
fn wlroots_include_dir(lib: &pkg_config::Library) -> PathBuf {
    lib.include_paths
        .iter()
        .find(|path| path.join("wlr").join("version.h").is_file())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "{WLROOTS_PC} was found, but none of its include paths contain \
                 wlr/version.h: {:?}",
                lib.include_paths
            )
        })
}

fn include_flags(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| format!("-I{}", path.display()))
        .collect()
}

/// Probe a supporting library for its include paths only. wlroots already links
/// everything it needs, so cargo link metadata is suppressed here.
///
/// A miss is not fatal: several of these ship headers in the default search path
/// with no `.pc` file, and on a standard prefix their `-I` is already covered by
/// wlroots' own. But if the header really is absent, bindgen fails several
/// seconds later with a bare clang "file not found" that names neither the
/// subsystem nor the package to install — so warn here, where that context still
/// exists.
fn probe_include_paths(name: &str, requested_by: Option<&str>) -> Vec<String> {
    match pkg_config::Config::new().cargo_metadata(false).probe(name) {
        Ok(lib) => include_flags(&lib.include_paths),
        Err(err) => {
            let who = requested_by
                .map(|f| format!("feature `{f}`"))
                .unwrap_or_else(|| "wlroots' unconditional headers".to_owned());
            println!(
                "cargo::warning={who} needs the `{name}` headers, but pkg-config could not \
                 find `{name}`. If the build fails next with a clang `file not found`, install \
                 the {name} development package. pkg-config said: {err}"
            );
            Vec::new()
        }
    }
}

/// Read a `have_*` flag out of `wlroots-0.20.pc`.
///
/// `pkg-config-rs` exposes only the `-D` defines from `Cflags`, not arbitrary
/// `.pc` variables, so this goes through its `get_variable` helper. That matters
/// for more than convenience: `get_variable` routes through the same
/// `Config::run` as `probe_wlroots`, so it resolves the `pkg-config` executable
/// and the `PKG_CONFIG_PATH`/`LIBDIR`/`SYSROOT_DIR` search paths through
/// pkg-config-rs's full targeted-env-var chain. Spawning `pkg-config` by hand
/// here would read the *host* `.pc` under a cross-compile while `probe_wlroots`
/// read the target's, and the resulting `have_*` values would describe a
/// different library than the one being linked.
///
/// A failure here is environmental, not "the subsystem is absent" — pkg-config
/// already succeeded once in `probe_wlroots` — so it panics rather than quietly
/// reporting the subsystem as unavailable and telling the user to rebuild
/// wlroots for no reason.
fn have_flag(var: &str) -> bool {
    match pkg_config::get_variable(WLROOTS_PC, var) {
        // wlroots writes these as the literal strings `true` / `false`. An
        // absent variable yields an empty string, which is not `true`.
        Ok(value) => value.trim() == "true",
        Err(err) => panic!(
            "failed to read `{var}` from {WLROOTS_PC}, even though the package itself \
             resolved. This is an environment problem, not a missing wlroots subsystem.\n\n\
             pkg-config said: {err}"
        ),
    }
}

/// Run `wayland-scanner server-header` over the vendored wlr-protocols XMLs.
fn generate_protocol_headers(out_dir: &Path) -> PathBuf {
    let dir = out_dir.join("protocol-include");
    fs::create_dir_all(&dir).expect("failed to create protocol include dir");

    // The `.pc` supplies an absolute path on every normal install. Reject a
    // relative one: `Command::new` would resolve it against the build script's
    // CWD (the crate root), so a `.pc` planted earlier in PKG_CONFIG_PATH could
    // point at a repo-local file and get it executed during `cargo build`.
    let scanner = pkg_config::get_variable("wayland-scanner", "wayland_scanner")
        .ok()
        .filter(|s| Path::new(s).is_absolute())
        .unwrap_or_else(|| "wayland-scanner".to_owned());
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset"));

    for proto in PROTOCOLS {
        let xml = manifest_dir.join("protocol").join(format!("{proto}.xml"));
        let header = dir.join(format!("{proto}-protocol.h"));
        let status = Command::new(&scanner)
            .arg("server-header")
            .arg(&xml)
            .arg(&header)
            .status()
            .unwrap_or_else(|err| {
                panic!(
                    "failed to run `{scanner}`: {err}\n\
                     wlr-sys needs the wayland-scanner binary at build time \
                     (package `wayland` on Arch, `libwayland-bin` on Debian)."
                )
            });
        assert!(
            status.success(),
            "wayland-scanner failed on {}",
            xml.display()
        );
        // A scanner too old for the XML can exit 0 having written nothing.
        // Catch that here rather than several seconds later as an opaque clang
        // "file not found" inside OUT_DIR.
        let written = fs::metadata(&header).map(|m| m.len()).unwrap_or(0);
        assert!(
            written > 0,
            "`{scanner}` exited successfully but wrote no output for {}. \
             The scanner may be too old for this protocol XML; check \
             `{scanner} --version`.",
            xml.display()
        );
    }
    dir
}

/// Every `wlr/**/*.h`, minus the gated headers that are not currently enabled.
///
/// Scanning rather than hardcoding means a wlroots 0.20.x patch release that adds
/// a header is picked up without touching this crate. The flip side is that the
/// gate list is static while the scan is dynamic, so a renamed or moved gated
/// header would stop matching and silently become unconditional — pulling in
/// `<vulkan/vulkan_core.h>` or `<xcb/xcb.h>` on machines that have neither. The
/// assertion below turns that into a build error naming the file to fix.
fn collect_headers(include_dir: &Path, enabled_gated: &BTreeSet<&str>) -> Vec<String> {
    let all_gated: BTreeSet<&str> = SUBSYSTEMS
        .iter()
        .flat_map(|s| s.headers.iter().copied())
        .collect();
    for gated in &all_gated {
        assert!(
            include_dir.join(gated).is_file(),
            "gated header `{gated}` no longer exists in {WLROOTS_PC} \
             ({}). Update SUBSYSTEMS in build.rs — leaving it stale would make the \
             header unconditional.",
            include_dir.display()
        );
    }

    let mut headers = Vec::new();
    let mut stack = vec![include_dir.join("wlr")];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|err| {
                panic!("failed to read an entry in {}: {err}", dir.display())
            });
            // `file_type()` does not follow symlinks, unlike `Path::is_dir`. A
            // symlinked directory under the include tree would otherwise let the
            // walk escape it, or loop forever if it points at an ancestor.
            let file_type = entry
                .file_type()
                .unwrap_or_else(|err| panic!("failed to stat {}: {err}", entry.path().display()));
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() || path.extension() != Some(OsStr::new("h")) {
                continue;
            }
            let rel = path
                .strip_prefix(include_dir)
                .expect("header outside include dir")
                .to_str()
                .expect("non-UTF-8 header path")
                .replace('\\', "/");
            if all_gated.contains(rel.as_str()) && !enabled_gated.contains(rel.as_str()) {
                continue;
            }
            headers.push(rel);
        }
    }
    headers.sort();
    headers
}

fn write_wrapper(out_dir: &Path, headers: &[String]) -> PathBuf {
    let mut src = String::from("/* Generated by wlr-sys build.rs. Do not edit. */\n");
    for header in headers {
        src.push_str(&format!("#include <{header}>\n"));
    }
    let path = out_dir.join("wrapper.h");
    fs::write(&path, src).expect("failed to write wrapper.h");
    path
}

fn generate_bindings(wrapper: &Path, clang_args: &[String], out_dir: &Path, bind_libinput: bool) {
    let mut builder = bindgen::Builder::default()
        .header(wrapper.to_str().expect("non-UTF-8 OUT_DIR"))
        .clang_args(clang_args)
        .allowlist_item("wlr_.*")
        .allowlist_item("WLR_.*")
        // Bitflag-style enums (wlr_edges, wlr_output_state_field, ...) routinely
        // carry values outside the declared variants, which makes a Rust `enum`
        // unsound across FFI. Newtypes keep the type distinction without it.
        .default_enum_style(bindgen::EnumVariation::NewType {
            is_bitfield: false,
            is_global: false,
        })
        // Layout assertions are this crate's primary safety net for the
        // blocklisted ecosystem types — pin the default so an upgrade cannot
        // silently drop them.
        .layout_tests(true)
        .prepend_enum_name(false)
        .raw_line("use wayland_sys::common::*;")
        .raw_line("use wayland_sys::server::*;")
        .raw_line("use xkbcommon_sys::*;");

    let mut blocklist: Vec<&str> = WAYLAND_COMMON_TYPES
        .iter()
        .chain(WAYLAND_SERVER_TYPES)
        .chain(XKB_TYPES)
        .copied()
        .collect();

    if bind_libinput {
        builder = builder.raw_line("use input_sys::*;");
        blocklist.extend_from_slice(INPUT_TYPES);
    }

    for ty in blocklist {
        builder = builder.blocklist_type(format!("^{ty}$"));
    }

    builder
        .generate()
        .expect("bindgen failed to generate wlroots bindings")
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}

fn feature_enabled(feature: &str) -> bool {
    let var = format!("CARGO_FEATURE_{}", feature.to_uppercase().replace('-', "_"));
    env::var_os(var).is_some()
}
