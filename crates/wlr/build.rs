//! Two jobs, both of which exist because information does not cross a package
//! boundary on its own.
//!
//! 1. **Subsystem cfgs.** `cargo::rustc-cfg` applies only to the package whose
//!    build script emitted it, so `wlr-sys`' `wlr_has_*` cfgs are invisible
//!    here. `wlr-sys` publishes the same reconciliation result as `links`
//!    metadata (`DEP_WLROOTS_HAVE_*`), which this script reads back and
//!    re-emits. Without it a `#[cfg(wlr_has_vulkan_renderer)]` module compiles
//!    to nothing, silently and with no error.
//!
//! 2. **pixman.** `pixman-1` is in wlroots' `Requires.private`, so it is not on
//!    our link line, but `region.rs` declares `pixman_region32_*` directly
//!    (using the `wlr-sys` types, so the identity that matters is preserved).
//!    Probing it here puts `-lpixman-1` where those declarations can resolve.

use std::env;

/// `(links-metadata env var, cfg)` for every wlroots subsystem.
///
/// This table must mirror `SUBSYSTEMS` in `crates/wlr-sys/build.rs` exactly —
/// the env var is `DEP_WLROOTS_` plus the uppercased `pc_var`, and the cfg is
/// the `cfg` field. A row missing here is not an error anywhere: the cfg simply
/// never becomes true and its module disappears. Add rows in the same order.
const SUBSYSTEM_CFGS: &[(&str, &str)] = &[
    ("DEP_WLROOTS_HAVE_DRM_BACKEND", "wlr_has_drm_backend"),
    ("DEP_WLROOTS_HAVE_X11_BACKEND", "wlr_has_x11_backend"),
    (
        "DEP_WLROOTS_HAVE_LIBINPUT_BACKEND",
        "wlr_has_libinput_backend",
    ),
    ("DEP_WLROOTS_HAVE_SESSION", "wlr_has_session"),
    ("DEP_WLROOTS_HAVE_GLES2_RENDERER", "wlr_has_gles2_renderer"),
    (
        "DEP_WLROOTS_HAVE_VULKAN_RENDERER",
        "wlr_has_vulkan_renderer",
    ),
    ("DEP_WLROOTS_HAVE_XWAYLAND", "wlr_has_xwayland"),
    (
        "DEP_WLROOTS_HAVE_COLOR_MANAGEMENT",
        "wlr_has_color_management",
    ),
    ("DEP_WLROOTS_HAVE_GBM_ALLOCATOR", "wlr_has_gbm_allocator"),
    (
        "DEP_WLROOTS_HAVE_UDMABUF_ALLOCATOR",
        "wlr_has_udmabuf_allocator",
    ),
];

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    for (_, cfg) in SUBSYSTEM_CFGS {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
    for (key, cfg) in SUBSYSTEM_CFGS {
        println!("cargo::rerun-if-env-changed={key}");
        // `wlr-sys` emits `cargo::metadata=<pc_var>=<bool>`, so the value is
        // the word "true", not "1".
        if env::var(key).as_deref() == Ok("true") {
            println!("cargo::rustc-cfg={cfg}");
        }
    }

    // docs.rs has no wlroots and no pixman; `wlr-sys` documents itself there
    // from a committed bindings snapshot, and this crate must stay documentable
    // alongside it. Nothing links during a doc build, so skipping the probe
    // costs nothing and a hard failure here would take `wlr`'s front page down.
    println!("cargo::rerun-if-env-changed=DOCS_RS");
    if env::var_os("DOCS_RS").is_some() {
        return;
    }

    // pkg-config emits the link directives itself.
    pkg_config::Config::new()
        .probe("pixman-1")
        .expect("wlr requires pixman-1; it is a hard dependency of wlroots itself");
}
