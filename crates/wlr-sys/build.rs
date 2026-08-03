//! Build script for `wlr-sys`.
//!
//! Locates wlroots 0.20 via pkg-config, reconciles the crate's Cargo features
//! against the subsystems the installed library was actually compiled with,
//! generates the two `wlr-protocols` server headers that wlroots' public headers
//! `#include` but do not ship, and runs bindgen over the result.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The pkg-config module name. wlroots embeds its minor version here because it
/// has no stable ABI: `libwlroots-0.20.so` and `libwlroots-0.21.so` coexist.
const WLROOTS_PC: &str = "wlroots-0.20";
const WLROOTS_MINOR: &str = "0.20";

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
        cfg: "wlr_has_drm_backend",
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
        cfg: "wlr_has_libinput_backend",
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
    // `wlr/render/color.h` is always bindable; this flag only says whether ICC
    // profile support was compiled in, so the subsystem gates no header.
    Subsystem {
        feature: Some("color-management"),
        pc_var: "have_color_management",
        cfg: "wlr_has_color_management",
        headers: &[],
        extra_pc: &[],
    },
    // Detect-only: no dedicated public header, so no Cargo feature.
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
/// enums (`wl_output_transform`, `wl_seat_capability`, `wl_shm_format`, ...) are
/// *not* provided by `wayland-sys` and must be generated locally.
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

// `drmModeModeInfo` is deliberately *not* blocklisted. It is libdrm's userspace
// struct from <xf86drmMode.h>, whereas `drm-sys` only generates the kernel uAPI
// header and exposes `drm_mode_modeinfo`. Those are distinct types that merely
// happen to agree on layout; equating them silently would be a claim this crate
// cannot verify, so it is generated locally instead (same rationale as pixman).

const XKB_TYPES: &[&str] = &[
    "xkb_context",
    "xkb_keymap",
    "xkb_state",
    "xkb_rule_names",
    "xkb_keysym_t",
    "xkb_mod_mask_t",
    "xkb_led_mask_t",
    "xkb_layout_index_t",
    "xkb_mod_index_t",
];

const INPUT_TYPES: &[&str] = &["libinput", "libinput_device", "libinput_event"];

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_SYSROOT_DIR");
    for cfg in SUBSYSTEMS.iter().map(|s| s.cfg) {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR unset"));
    let wlroots = probe_wlroots();
    let include_dir = wlroots_include_dir(&wlroots);

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
    clang_args.extend(probe_include_paths("egl"));

    let mut gated_headers: BTreeSet<&str> = BTreeSet::new();
    let mut enabled_cfgs: Vec<&str> = Vec::new();

    for sub in SUBSYSTEMS {
        let available = pc_flag(sub.pc_var);
        // Detect-only subsystems are governed purely by the installed library.
        let requested = sub.feature.map_or(available, feature_enabled);

        if requested && !available {
            let feature = sub.feature.unwrap_or("<detect-only>");
            println!(
                "cargo::warning=feature `{feature}` is enabled, but the installed \
                 {WLROOTS_PC} reports {}=false; disabling it. Rebuild wlroots with that \
                 subsystem, or turn the feature off to silence this warning.",
                sub.pc_var
            );
            continue;
        }
        if !requested {
            continue;
        }

        enabled_cfgs.push(sub.cfg);
        gated_headers.extend(sub.headers.iter().copied());
        for pc in sub.extra_pc {
            clang_args.extend(probe_include_paths(pc));
        }
    }

    // `wlr/backend/drm.h` and `wlr/backend/libinput.h` both include
    // `wlr/backend/session.h`, so bind it whenever either is on.
    if enabled_cfgs.contains(&"wlr_has_drm_backend")
        || enabled_cfgs.contains(&"wlr_has_libinput_backend")
    {
        gated_headers.insert("wlr/backend/session.h");
    }

    for cfg in &enabled_cfgs {
        println!("cargo::rustc-cfg={cfg}");
    }

    let all_gated: BTreeSet<&str> = SUBSYSTEMS
        .iter()
        .flat_map(|s| s.headers.iter().copied())
        .collect();
    let headers = collect_headers(&include_dir, &all_gated, &gated_headers);
    let wrapper = write_wrapper(&out_dir, &headers);
    println!("cargo::rerun-if-changed={}", include_dir.display());

    let bind_libinput = enabled_cfgs.contains(&"wlr_has_libinput_backend");
    generate_bindings(&wrapper, &clang_args, &out_dir, bind_libinput);
}

/// Locate wlroots 0.20, with actionable errors for the two common failures.
fn probe_wlroots() -> pkg_config::Library {
    match pkg_config::Config::new()
        .range_version(WLROOTS_MINOR.."0.21")
        .probe(WLROOTS_PC)
    {
        Ok(lib) => lib,
        Err(err) => {
            let installed = pkg_config::Config::new()
                .cargo_metadata(false)
                .probe(WLROOTS_PC)
                .ok()
                .map(|lib| lib.version);
            match installed {
                Some(version) => panic!(
                    "wlr-sys {} binds wlroots {WLROOTS_MINOR}.x, but {WLROOTS_PC} reports \
                     version {version}. wlroots has no stable ABI: use the wlr-sys release \
                     whose minor version matches your installed wlroots minor version.",
                    env!("CARGO_PKG_VERSION"),
                ),
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
/// with no `.pc` file. If a header really is absent, bindgen fails next with a
/// precise "file not found".
fn probe_include_paths(name: &str) -> Vec<String> {
    pkg_config::Config::new()
        .cargo_metadata(false)
        .probe(name)
        .map(|lib| include_flags(&lib.include_paths))
        .unwrap_or_default()
}

/// Read a `have_*` variable from the `.pc` file. wlroots writes them as the
/// literal strings `true` / `false`. `pkg-config-rs` cannot read arbitrary
/// variables, so ask the tool directly.
fn pc_flag(var: &str) -> bool {
    pkg_config_variable(WLROOTS_PC, var).as_deref() == Some("true")
}

fn pkg_config_variable(package: &str, var: &str) -> Option<String> {
    let output = Command::new(env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".into()))
        .arg(format!("--variable={var}"))
        .arg(package)
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Run `wayland-scanner server-header` over the vendored wlr-protocols XMLs.
fn generate_protocol_headers(out_dir: &Path) -> PathBuf {
    let dir = out_dir.join("protocol-include");
    fs::create_dir_all(&dir).expect("failed to create protocol include dir");

    let scanner = pkg_config_variable("wayland-scanner", "wayland_scanner")
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
    }
    dir
}

/// Every `wlr/**/*.h`, minus the gated headers that are not currently enabled.
///
/// Scanning rather than hardcoding means a wlroots 0.20.x patch release that adds
/// a header is picked up without touching this crate.
fn collect_headers(
    include_dir: &Path,
    all_gated: &BTreeSet<&str>,
    enabled_gated: &BTreeSet<&str>,
) -> Vec<String> {
    let mut headers = Vec::new();
    let mut stack = vec![include_dir.join("wlr")];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()));
        for entry in entries {
            let path = entry.expect("failed to read directory entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension() != Some(OsStr::new("h")) {
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
        writeln!(src, "#include <{header}>").expect("writing to a String cannot fail");
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
        .derive_default(false)
        .generate_comments(true)
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
    let var = format!(
        "CARGO_FEATURE_{}",
        feature.to_uppercase().replace(['-', '.'], "_")
    );
    env::var_os(var).is_some()
}
