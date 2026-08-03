# wlr-sys

Raw FFI bindings to [wlroots](https://gitlab.freedesktop.org/wlroots/wlroots) 0.19.

This crate exposes wlroots exactly as C sees it — raw pointers, no lifetimes, no
`Drop`, no safety. Safe abstractions belong in a wrapper crate layered on top.

## Versioning

**The crate's minor version tracks the wlroots minor version.** `wlr-sys 0.19.x`
binds wlroots 0.19.x, and nothing else. wlroots has no stable ABI and ships a
version-suffixed soname (`libwlroots-0.19.so`), so a build against a different
minor is rejected up front rather than miscompiling silently.

Two consequences worth knowing before you depend on this crate:

- **Within a minor, the hand-written API is frozen.** Under cargo's 0.x rules
  `0.19.0 → 0.19.1` is an automatic upgrade, so there is no version in which to
  make a breaking change to `wl_list_iter`, the macros, the feature names, the
  cfg names, or the type blocklist. Those changes wait for the next wlroots minor.
- **The bound API is whatever your wlroots 0.19.x exposes.** `build.rs` scans the
  installed include tree, so code using a symbol added in a wlroots patch release
  will not compile for users on an older patch — with a plain "cannot find
  function" and no hint that the patch level is the cause.

## Requirements

| Requirement | Why |
|---|---|
| wlroots 0.19.x + headers (`wlroots-0.19.pc`) | The library being bound — `libwlroots-0.19-dev` on Ubuntu 26.04 |
| `libclang` | bindgen runs at build time |
| `wayland-scanner` | Regenerates two protocol headers wlroots does not install |

Two of wlroots' public headers `#include` generated `wlr-protocols` headers that
are private to the wlroots build and never installed. This crate vendors the
protocol XML under `protocol/` and regenerates them into `OUT_DIR`.

Bindings are generated at build time. docs.rs has no wlroots installation, so
`build.rs` falls back there to a committed snapshot of the all-features bindings
(`prebuilt/bindings-docsrs.rs`); CI regenerates it and fails on drift, so the
published documentation stays honest. Locally, `cargo doc -p wlr-sys --open`
documents what *your* machine actually has.

## Features

Every optional wlroots subsystem is a compile-time flag baked into the installed
library. A subsystem is bound only if its Cargo feature is on *and* the installed
library reports it available via its pkg-config `have_*` variable. A mismatch is
a build **warning** and the subsystem is disabled — a distro rebuilding wlroots
without Xwayland degrades your build rather than breaking it.

| Feature | Default | pkg-config variable | cfg / `DEP_WLROOTS_*` |
|---|---|---|---|
| `drm-backend` | yes | `have_drm_backend` | `wlr_has_drm_backend` |
| `x11-backend` | yes | `have_x11_backend` | `wlr_has_x11_backend` |
| `libinput-backend` | yes | `have_libinput_backend` | `wlr_has_libinput_backend` |
| `session` | yes | `have_session` | `wlr_has_session` |
| `gles2-renderer` | yes | `have_gles2_renderer` | `wlr_has_gles2_renderer` |
| `xwayland` | yes | `have_xwayland` | `wlr_has_xwayland` |
| `vulkan-renderer` | **no** | `have_vulkan_renderer` | `wlr_has_vulkan_renderer` |
| — | — | `have_color_management` | `wlr_has_color_management` |
| — | — | `have_gbm_allocator` | `wlr_has_gbm_allocator` |
| — | — | `have_udmabuf_allocator` | `wlr_has_udmabuf_allocator` |

`vulkan-renderer` is off by default because `wlr/render/vulkan.h` includes
`<vulkan/vulkan_core.h>`, and the Vulkan headers are a separate package that
wlroots does not pull in (`vulkan-headers` on Arch, `libvulkan-dev` on Debian).

`drm-backend` and `libinput-backend` both imply `session`, because their headers
`#include <wlr/backend/session.h>`. That keeps the cfg and the bound symbols in
agreement — you cannot end up with `wlr_session_*` linkable but
`wlr_has_session` unset.

The last three rows have no Cargo feature. They gate no public header, so a
feature could only ever *suppress* a true detection result — a knob whose
reachable states are "correct" and "lying". They are detected and reported only.

### Reading the result from your own crate

Cargo does **not** propagate `rustc-cfg` to dependents, so `#[cfg(wlr_has_xwayland)]`
in your crate would silently be false no matter what this crate detected. The
values travel as `links` metadata instead. Add four lines to your `build.rs`:

```rust,ignore
fn main() {
    for var in ["xwayland", "drm_backend", "vulkan_renderer"] {
        println!("cargo::rustc-check-cfg=cfg(wlr_has_{var})");
        if std::env::var(format!("DEP_WLROOTS_HAVE_{}", var.to_uppercase())).as_deref() == Ok("true") {
            println!("cargo::rustc-cfg=wlr_has_{var}");
        }
    }
}
```

after which the cfgs work as expected:

```rust,ignore
#[cfg(wlr_has_xwayland)]
let xwayland = unsafe { wlr_sys::wlr_xwayland_create(display, compositor, true) };
```

Every row above is published as `DEP_WLROOTS_<PC_VARIABLE>`, set to `true` or
`false` — including for subsystems that were disabled, so you can distinguish
"absent" from "never asked". Test the metadata rather than the Cargo feature:
features are additive and unified across the graph, while the metadata reflects
what was actually bound on the build machine.

Note that the mismatch warning above is a `cargo::warning`, which cargo **hides
for registry dependencies**. If you need a missing subsystem to be fatal, check
the `DEP_WLROOTS_*` value in your build script and `panic!`.

## Ecosystem types

Types that cross into other libraries are re-exported rather than regenerated, so
a `*mut wl_display` here is the same Rust type as everywhere else in the
wayland-rs ecosystem:

- [`wayland-sys`] — `wl_display`, `wl_client`, `wl_resource`, `wl_listener`,
  `wl_signal`, `wl_list`, `wl_array`, `wl_event_loop`, ...
- [`xkbcommon-sys`] — `xkb_keymap`, `xkb_state`, `xkb_keysym_t`, ...
- [`input-sys`] — `libinput_device`, ... (only with `libinput-backend`)

Two families are generated locally instead, for different reasons:

- **pixman.** `pixman_region32_t` is embedded *by value* in 17 wlroots struct
  fields, so a mismatched layout would be silent memory corruption rather than a
  compile error — and `pixman-sys` is unmaintained at 0.1.0, offering no type
  whose identity can be verified.
- **`drmModeModeInfo`.** wlroots only forward-declares it and passes it by
  pointer, so it is bound as an opaque type. `drm-sys` would not help regardless:
  it generates the kernel uAPI header and exposes the distinct
  `drm_mode_modeinfo`, whereas wlroots means libdrm's userspace struct from
  `<xf86drmMode.h>`.

Core Wayland protocol enums are generated locally too, since `wayland-sys` does
not provide them — but only the ones a wlroots declaration actually reaches. In
the default configuration that is eight: `wl_output_transform`,
`wl_output_subpixel`, `wl_pointer_axis`, `wl_pointer_axis_source`,
`wl_pointer_axis_relative_direction`, `wl_pointer_button_state`,
`wl_keyboard_key_state` and `wl_data_device_manager_dnd_action`. Enums no wlroots
header references (`wl_seat_capability`, `wl_shm_format`, …) are not generated at
all.

libc types (`timespec`, `dev_t`, …) are likewise generated locally, so
`wlr_sys::timespec` is *not* `libc::timespec`. This crate deliberately does not
depend on `libc`.

`tests/interop.rs` pins these at compile time by coercing real wlroots functions
to signatures written in the ecosystem crates' types. It covers the nominal types
(structs and enums); `xkb_*_t` aliases are `= u32` and therefore transparent, so
no check written against them could fail — they are documented there rather than
asserted.

## Events

wlroots publishes events as `wl_signal`s and you subscribe with a `wl_listener`.
The functions that do this are `static inline` in `wayland-server-core.h`, so no
symbol exists to link against. This crate supplies Rust equivalents:
`wl_signal_init`, `wl_signal_add` and `wl_signal_get` are re-exported from
`wayland-sys`, and `wl_signal_emit_mutable` is declared here (it is a real
exported symbol that `wayland-sys` does not bind). `wl_list_for_each!` iterates
an intrusive list, and `container_of!` is `wl_container_of`.

The `container_of!` macro documentation carries the callback pattern every
wlroots consumer needs — recovering the owning struct from the `*mut wl_listener`
a callback is handed.

## Verification

bindgen emits several hundred `const _` layout assertions — the exact count
varies with the feature set — checked on every build rather than only under
`cargo test`. They are the safety net for the re-exported types above: if
`wayland-sys`'s `wl_list` layout ever diverged, every wlroots struct embedding
it would fail to compile.

| Check | What it proves |
|---|---|
| `tests/interop.rs` | wlroots' API really speaks `wayland-sys` / `xkbcommon-sys` / `input-sys` types, not local look-alikes |
| `tests/subsystems.rs` | When a `wlr_has_*` cfg is set, that subsystem's symbols were actually bound |
| `tests/link.rs` | The linked `libwlroots-0.19.so` matches the headers bindgen read, down to the patch version |
| `tests/signal.rs` | Hand-written `wl_signal` / `wl_list` / `container_of!` against real libwayland |
| `examples/headless.rs` | End-to-end backend bring-up and teardown; needs no GPU or seat |

```sh
cargo run -p wlr-sys --example headless
```

## License

MIT. The vendored protocol XML under `protocol/` is from
[wlr-protocols](https://gitlab.freedesktop.org/wlroots/wlr-protocols) and carries
its own MIT-style notices.

[`wayland-sys`]: https://crates.io/crates/wayland-sys
[`xkbcommon-sys`]: https://crates.io/crates/xkbcommon-sys
[`input-sys`]: https://crates.io/crates/input-sys
