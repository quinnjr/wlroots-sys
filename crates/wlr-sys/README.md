# wlr-sys

Raw FFI bindings to [wlroots](https://gitlab.freedesktop.org/wlroots/wlroots) 0.20.

This crate exposes wlroots exactly as C sees it — raw pointers, no lifetimes, no
`Drop`, no safety. Safe abstractions belong in a wrapper crate layered on top.

## Versioning

**The crate's minor version tracks the wlroots minor version.** `wlr-sys 0.20.x`
binds wlroots 0.20.x, and nothing else. wlroots has no stable ABI and ships a
version-suffixed soname (`libwlroots-0.20.so`), so a build against a different
minor is rejected up front rather than miscompiling silently.

## Requirements

| Requirement | Why |
|---|---|
| wlroots 0.20.x + headers (`wlroots-0.20.pc`) | The library being bound |
| `libclang` | bindgen runs at build time |
| `wayland-scanner` | Regenerates two protocol headers wlroots does not install |

Two of wlroots' public headers `#include` generated `wlr-protocols` headers that
are private to the wlroots build and never installed. This crate vendors the
protocol XML under `protocol/` and regenerates them into `OUT_DIR`.

Bindings are generated at build time with no committed fallback, so **docs.rs
cannot build this crate**. Use `cargo doc -p wlr-sys --open` locally.

## Features

Every optional wlroots subsystem is a compile-time flag baked into the installed
library. A subsystem is bound only if its Cargo feature is on *and* the installed
library reports it available via its pkg-config `have_*` variable. A mismatch is
a build **warning** and the subsystem is disabled — a distro rebuilding wlroots
without Xwayland degrades your build rather than breaking it.

Because features and reality can differ, downstream code should test the emitted
`cfg`s, not the features:

```rust,ignore
#[cfg(wlr_has_xwayland)]
let xwayland = unsafe { wlr_sys::wlr_xwayland_create(display, compositor, true) };
```

| Feature | Default | pkg-config variable | cfg |
|---|---|---|---|
| `xwayland` | yes | `have_xwayland` | `wlr_has_xwayland` |
| `drm-backend` | yes | `have_drm_backend` | `wlr_has_drm_backend` |
| `x11-backend` | yes | `have_x11_backend` | `wlr_has_x11_backend` |
| `libinput-backend` | yes | `have_libinput_backend` | `wlr_has_libinput_backend` |
| `gles2-renderer` | yes | `have_gles2_renderer` | `wlr_has_gles2_renderer` |
| `session` | yes | `have_session` | `wlr_has_session` |
| `color-management` | yes | `have_color_management` | `wlr_has_color_management` |
| `vulkan-renderer` | **no** | `have_vulkan_renderer` | `wlr_has_vulkan_renderer` |
| — | — | `have_gbm_allocator` | `wlr_has_gbm_allocator` |
| — | — | `have_udmabuf_allocator` | `wlr_has_udmabuf_allocator` |

`vulkan-renderer` is off by default because `wlr/render/vulkan.h` includes
`<vulkan/vulkan_core.h>`, and the Vulkan headers are a separate package that
wlroots does not pull in (`vulkan-headers` on Arch, `libvulkan-dev` on Debian).

The last two rows have no Cargo feature: they gate no public header, so they are
detected and surfaced as `cfg`s only.

## Ecosystem types

Types that cross into other libraries are re-exported rather than regenerated, so
a `*mut wl_display` here is the same Rust type as everywhere else in the
wayland-rs ecosystem:

- [`wayland-sys`] — `wl_display`, `wl_client`, `wl_resource`, `wl_listener`,
  `wl_signal`, `wl_list`, `wl_array`, `wl_event_loop`, ...
- [`xkbcommon-sys`] — `xkb_keymap`, `xkb_state`, `xkb_keysym_t`, ...
- [`input-sys`] — `libinput_device`, ... (only with `libinput-backend`)

Two families are generated locally instead. In both cases a mismatched layout
would be silent memory corruption rather than a compile error, and no maintained
crate offers a type whose identity can be verified:

- **pixman.** `pixman-sys` is unmaintained at 0.1.0, and `pixman_region32_t` is
  embedded *by value* in wlroots structs.
- **`drmModeModeInfo`.** This is libdrm's userspace struct from
  `<xf86drmMode.h>`. `drm-sys` generates only the kernel uAPI header and exposes
  the distinct `drm_mode_modeinfo`.

The core Wayland protocol enums (`wl_output_transform`, `wl_seat_capability`,
`wl_shm_format`, ...) are generated locally too — `wayland-sys` does not have them.

## Events

wlroots publishes events as `wl_signal`s and you subscribe with a `wl_listener`.
The functions that do this are `static inline` in `wayland-server-core.h`, so no
symbol exists to link against. This crate supplies Rust equivalents:

```rust,ignore
use std::ffi::c_void;
use wayland_sys::server::wl_listener;

#[repr(C)]
struct Output {
    wlr_output: *mut wlr_sys::wlr_output,
    frame: wl_listener,
}

unsafe extern "C" fn on_frame(listener: *mut wl_listener, _data: *mut c_void) {
    let output: *mut Output = unsafe { wlr_sys::container_of!(listener, Output, frame) };
    // ...
}

unsafe {
    (*output).frame.notify = on_frame;
    wlr_sys::wl_signal_add(&raw mut (*wlr_output).events.frame, &raw mut (*output).frame);
}
```

`wl_signal_init`, `wl_signal_add` and `wl_signal_get` are re-exported from
`wayland-sys`; `wl_signal_emit_mutable` is declared here (it is a real exported
symbol that `wayland-sys` does not bind). `wl_list_for_each!` iterates an
intrusive list, and `container_of!` is `wl_container_of`.

## Verification

- **687 compile-time layout assertions** generated by bindgen. These are `const`
  assertions, so they are checked on every build, not only under `cargo test`.
  They are the safety net for the re-exported types above: if `wayland-sys`'s
  `wl_list` layout ever diverged, every wlroots struct embedding it fails to
  compile.
- `tests/link.rs` — asserts the linked `libwlroots-0.20.so` reports the same
  version as the headers bindgen read.
- `tests/signal.rs` — exercises the hand-written `wl_signal`, `wl_list` and
  `container_of!` code against real libwayland.
- `examples/headless.rs` — creates a `wl_display`, starts the headless backend,
  dispatches the event loop, and tears down. Needs no GPU or seat:

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
