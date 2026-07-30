# `wlr-sys` — raw FFI bindings to wlroots 0.20

**Date:** 2026-07-29
**Status:** Approved
**Repo:** `wlroots-sys` (directory/remote name unchanged)

## Purpose

Provide raw, unsafe FFI bindings to wlroots 0.20 as the foundation of a two-crate
stack: `wlr-sys` (this document) and a safe wrapper crate `wlr` built on top of it
later. Design decisions here anticipate that wrapper, but no safe abstractions
ship in `wlr-sys` itself.

Both `wlroots-sys` and `wlroots` are taken on crates.io by abandoned crates
(`wlroots-sys` 0.16.0, Aug 2023; `wlroots` 0.4.0, 2019). The names `wlr-sys` and
`wlr` were verified available on 2026-07-29.

## Target environment

Verified against the development machine:

- wlroots **0.20.2**, pkg-config package `wlroots-0.20`
- Headers at `/usr/include/wlroots-0.20/wlr/`, 123 `.h` files
- All ten optional subsystems compiled in (`have_*=true` in the `.pc` file)

Header scan for bindgen hazards came back clean:

- **Zero** `static inline` functions in any `wlr/` header
- **Zero** bitfields
- 4 anonymous unions (`backend/session.h`, `types/wlr_ext_workspace_v1.h`,
  `types/wlr_xdg_shell.h` ×2)
- 2 `va_list` uses, both in `util/log.h`

Consequently **no C shim is required** — the crate has no `cc` build dependency.

## Architecture

### Repository structure

```
wlroots-sys/
├── Cargo.toml                  [workspace] members = ["crates/*"]
├── crates/
│   └── wlr-sys/
│       ├── Cargo.toml
│       ├── build.rs
│       └── src/
│           ├── lib.rs          include!(bindings.rs), re-exports, module wiring
│           ├── list.rs         wl_list iteration + container_of!
│           └── signal.rs       wl_signal_init/add/get/emit_mutable
├── examples/
│   └── headless.rs
└── docs/superpowers/specs/
```

`crates/wlr/` (the safe wrapper) is added later as a second workspace member.
Lockstep versioning and a single CI keep the wrapper from drifting from the
bindings it wraps — which matters because wlroots breaks API every minor release.

### Crate manifest

```toml
[package]
name    = "wlr-sys"
version = "0.20.0"
edition = "2024"
links   = "wlroots-0.20"
```

**Versioning policy: crate minor == wlroots minor.** `wlr-sys 0.20.x` binds
wlroots 0.20.x; `wlr-sys 0.21.x` binds wlroots 0.21.x. wlroots has no stable ABI
and uses a version-suffixed soname (`libwlroots-0.20.so`), so a single crate
release cannot span minors. The `links` key prevents two incompatible copies of
the bindings from coexisting in one dependency graph.

## `build.rs`

Five steps, in order.

### 1. Probe

```rust
pkg_config::Config::new()
    .range_version("0.20".."0.21")
    .probe("wlroots-0.20")
```

Failure modes, both hard errors with actionable messages:

- **Not installed** — error naming the pkg-config package and common distro
  package names.
- **Wrong minor installed** (0.19, 0.21, …) — error stating this crate binds
  0.20.x and naming the `wlr-sys` version that matches what is installed.

### 2. Read capabilities

The `.pc` file exposes each compile-time subsystem as a `true`/`false` string
variable (note: strings, not `1`/`0`):

| pkg-config variable | Cargo feature | cfg emitted |
|---|---|---|
| `have_drm_backend` | `drm-backend` | `wlr_has_drm_backend` |
| `have_x11_backend` | `x11-backend` | `wlr_has_x11_backend` |
| `have_libinput_backend` | `libinput-backend` | `wlr_has_libinput_backend` |
| `have_xwayland` | `xwayland` | `wlr_has_xwayland` |
| `have_gles2_renderer` | `gles2-renderer` | `wlr_has_gles2_renderer` |
| `have_vulkan_renderer` | `vulkan-renderer` | `wlr_has_vulkan_renderer` |
| `have_session` | `session` | `wlr_has_session` |
| `have_color_management` | `color-management` | `wlr_has_color_management` |
| `have_gbm_allocator` | *(none — cfg only)* | `wlr_has_gbm_allocator` |
| `have_udmabuf_allocator` | *(none — cfg only)* | `wlr_has_udmabuf_allocator` |

`gbm_allocator` and `udmabuf_allocator` get no Cargo feature because they gate no
dedicated public header; they are surfaced as cfgs for downstream use only.

### 3. Reconcile features against reality

A subsystem is bound **iff** its Cargo feature is enabled **and** the
corresponding `have_*` is `true`.

- Feature enabled, library lacks it → emit `cargo::warning` and disable. This is
  deliberately a warning, not an error, so a distro rebuilding wlroots without
  (say) Xwayland degrades the build instead of breaking it.
- Feature disabled → not bound regardless of library support. Keeps bindgen
  output and compile time down for minimal consumers.

Emit `cargo::rustc-check-cfg=cfg(wlr_has_*)` for every cfg in the table above,
then `cargo::rustc-cfg=wlr_has_*` for each enabled one.

### 4. Synthesize `$OUT_DIR/wrapper.h`

A single generated header containing `#include <wlr/...>` lines for all ~123
headers, with the subsystem-specific ones gated by the step-3 decisions.

### 5. Run bindgen

One bindgen invocation over `wrapper.h`:

- **Allowlist:** `wlr_.*`, `WLR_.*` (types, functions, and vars)
- **Blocklist:** external types, re-imported per the type-interop table below
- **Enum style:** `--default-enum-style=rust_non_exhaustive` — safe here because
  the header scan found no bitfields
- **Layout tests:** ON (see Testing)
- **Rerun triggers:** `rerun-if-changed` on the wlroots include directory and
  `rerun-if-env-changed=PKG_CONFIG_PATH`

Output goes to `$OUT_DIR/bindings.rs`; `lib.rs` `include!`s it into a private
module and re-exports flat, matching how C code uses wlroots (one namespace, no
`wlr/` path mirroring).

## Type interop

External types are blocklisted in bindgen and re-imported from ecosystem crates,
so `wlr-sys` types are compatible with wayland-rs and the rest of the ecosystem.
`wl_display`, `wl_listener`, and friends are the *same* Rust types everywhere.

| Types | Crate | Version |
|---|---|---|
| `wl_display`, `wl_client`, `wl_resource`, `wl_listener`, `wl_signal`, `wl_list`, `wl_array`, `wl_event_loop` | `wayland-sys` (`server` feature) | 0.31 |
| `drmModeModeInfo`, DRM format/modifier types | `drm-sys` | 0.8 |
| `xkb_keymap`, `xkb_state`, `xkb_context`, `xkb_mod_mask_t` | `xkbcommon-sys` | 1.4 |
| `libinput_device` and other `libinput_*` | `input-sys` | 1.19 |
| `size_t`, `timespec`, `dev_t`, `clockid_t` | `libc` | 0.2 |

`libseat` types appear only as opaque pointers in `backend/session.h`, so no
`libseat-sys` dependency is needed.

### Deviation: pixman types are generated locally

pixman is **not** blocklisted. `pixman-sys` is at 0.1.0 and unmaintained, while
`pixman_region32_t` is embedded **by value** in `wlr_surface` and damage-tracking
structs — a stale layout there is silent memory corruption, not a compile error.
bindgen already sees the pixman headers through wlroots' own `-I` flags, so
`pixman_region32_t` and `pixman_image_t` are generated inside `wlr-sys`.

**Accepted cost:** the safe `wlr` wrapper cannot pass a `pixman` crate region
directly to wlroots without a transmute. Revisit if `pixman-sys` becomes
actively maintained.

## Hand-written support modules

### `src/signal.rs`

`wl_signal_init`, `wl_signal_add`, `wl_signal_get`, and `wl_signal_emit_mutable`
are `static inline` in `wayland-server-core.h` — **no symbol exists to link
against**, and `wayland-sys` does not provide them. wlroots' entire event model
is `wl_signal_add(&thing->events.destroy, &my_listener)`, so the crate is
unusable without them.

Reimplemented in Rust against `wl_list_init`/`wl_list_insert`/`wl_list_remove`,
which *are* real exported symbols in `libwayland-server`.

### `src/list.rs`

- `container_of!(ptr, Type, field)` — the Rust equivalent of `wl_container_of`,
  required to recover the owning struct from a `*mut wl_listener` in every
  wlroots callback.
- An `Iterator` over an intrusive `wl_list`, replacing the `wl_list_for_each`
  macro.

### Logging

`wlr_log_init`'s callback signature takes a `va_list`. `wlr-sys` exposes it raw
and documents passing a null callback; formatting is the safe wrapper's problem.

## Features

```toml
[features]
default = ["xwayland", "drm-backend", "x11-backend", "libinput-backend",
           "gles2-renderer", "vulkan-renderer", "session", "color-management"]
```

Defaults match a stock distro build (all ten `have_*` are `true` on the target
machine), so the common case works with no configuration. Minimal consumers use
`default-features = false`.

## Error handling

| Condition | Behavior |
|---|---|
| wlroots not installed | Hard build error naming pkg-config package + distro packages |
| Wrong wlroots minor installed | Hard build error naming the matching `wlr-sys` version |
| Feature enabled, `have_*` false | `cargo::warning`, feature silently disabled |
| libclang missing | bindgen's own error; README documents the requirement |

## Testing

1. **bindgen layout tests** — generated `__bindgen_test_layout_*` tests run under
   `cargo test`. This is the primary safety net for the type-interop table: if
   `wayland-sys`'s `wl_list` layout ever diverged, every `wlr_*` struct embedding
   it fails loudly instead of corrupting memory.
2. **`tests/link.rs`** — calls `wlr_version_get_major/minor/micro` and asserts
   `0` / `20` / any. Proves the linked `.so` matches the headers bindgen read.
3. **`examples/headless.rs`** — `wl_display_create` → `wlr_headless_backend_create`
   → `wlr_backend_start` → one event-loop dispatch → teardown. End-to-end smoke
   test requiring no GPU and no seat.
4. **CI** — GitHub Actions on an Arch container (currently the only distro
   shipping wlroots 0.20): `fmt`, `clippy`, `test`, run the example.

## Out of scope (YAGNI)

- Vendored meson/ninja build of wlroots
- `dlopen`-based loading
- Multi-version support (0.19 / 0.21 / auto-detection)
- Any safe abstraction in `wlr-sys`: no `Drop`, no traits, no lifetimes — raw
  pointers only
- Wayland protocol code generation

## Accepted consequences

**docs.rs will not build this crate.** Bindings are generated at build time only,
with no checked-in fallback, and docs.rs has no wlroots installation and will not
run arbitrary package installs. `docs.rs/wlr-sys` will show a build failure, and
the same applies to the `wlr` wrapper unless it is `cfg`-guarded. This was chosen
knowingly over a committed-fallback `bindings.rs`; documentation is generated
locally with `cargo doc`.

**libclang is a hard build dependency** for every consumer of the crate.

**Consumers must have wlroots 0.20 installed.** There is no fallback path.
