# `wlr-sys` — raw FFI bindings to wlroots 0.20

**Date:** 2026-07-29
**Status:** Implemented — **historical record, not maintained**
**Repo:** `wlroots-sys` (directory/remote name unchanged)

> This is a dated design document plus the deviations found while building it.
> It is deliberately frozen: the measurements in it (header counts, assertion
> counts, pkg-config spawn counts) were taken on one machine on one day and are
> invalidated by any wlroots patch, bindgen bump, or feature change.
>
> **For current behaviour see [`crates/wlr-sys/README.md`](../../../crates/wlr-sys/README.md)
> (which is also the crate's rustdoc), the doc comments in `build.rs`, and
> [`docs/RELEASING.md`](../../RELEASING.md).** Several decisions recorded here
> were subsequently revised — notably the `links` value, the `color-management`
> feature, and how subsystem detection reaches downstream crates. Where this
> document and the README disagree, the README is right.

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

Two things the header scan did *not* predict, both found when the headers were
first compiled:

- Every wlroots header still `#error`s without **`-DWLR_USE_UNSTABLE`** in 0.20.
- `wlr/types/wlr_layer_shell_v1.h` and `wlr/types/wlr_output_power_management_v1.h`
  `#include` **generated `wlr-protocols` headers that wlroots never installs**.
  See [Vendored protocols](#vendored-protocols).

## Architecture

### Repository structure

```
wlroots-sys/
├── Cargo.toml                  [workspace] members = ["crates/*"]
├── crates/
│   └── wlr-sys/
│       ├── Cargo.toml
│       ├── build.rs
│       ├── README.md
│       ├── src/
│       │   ├── lib.rs          include!(bindings.rs), re-exports, module wiring
│       │   ├── list.rs         wl_list iteration + container_of!
│       │   └── signal.rs       wl_signal_init/add/get/emit_mutable
│       ├── protocol/           vendored wlr-protocols XML
│       ├── tests/              interop.rs, link.rs, signal.rs
│       └── examples/headless.rs
├── .github/workflows/ci.yml
└── docs/superpowers/specs/
```

(`examples/` and `tests/` live inside the crate, not at the repo root — cargo
only discovers them per-package.)

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

Six steps, in order.

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

One documented exception to the "iff": `wlr/backend/drm.h` and
`wlr/backend/libinput.h` both `#include <wlr/backend/session.h>`, so that header
is bound transitively whenever either backend is on, even if the `session`
feature is off. The `wlr_has_session` cfg still follows the strict rule, so with
`--no-default-features --features drm-backend` the `wlr_session_*` symbols are
declared while the cfg is absent. Downstream code should gate on the cfg.

Emit `cargo::rustc-check-cfg=cfg(wlr_has_*)` for every cfg in the table above,
then `cargo::rustc-cfg=wlr_has_*` for each enabled one.

Implementation detail: the `have_*` values are read by shelling out to
`pkg-config --variable=`, because `pkg-config-rs` exposes only `-D` defines from
`Cflags`, not arbitrary `.pc` variables.

### 4. Vendored protocols

`wlr/types/wlr_layer_shell_v1.h` and `wlr/types/wlr_output_power_management_v1.h`
`#include "wlr-layer-shell-unstable-v1-protocol.h"` and
`"wlr-output-power-management-unstable-v1-protocol.h"`. Those headers are
generated during the wlroots build and are **never installed**, so the public
headers cannot be compiled as shipped.

The crate vendors the two XML files from
[wlr-protocols](https://gitlab.freedesktop.org/wlroots/wlr-protocols) under
`protocol/` and regenerates the headers into `$OUT_DIR/protocol-include` with
`wayland-scanner server-header`. This makes the `wayland-scanner` binary a
build-time host requirement, alongside `libclang`.

### 5. Synthesize `$OUT_DIR/wrapper.h`

A single generated header containing `#include <wlr/...>` lines. The header list
is produced by **scanning `<includedir>/wlroots-0.20/wlr/**/*.h`** rather than
hardcoding, so a 0.20.x patch release that adds a header is picked up without
touching this crate; the subsystem-specific ones are then subtracted per step 3.
122 of the 123 headers are bound in the default configuration (`render/vulkan.h`
is the exception — see Features).

### 6. Run bindgen

One bindgen invocation over `wrapper.h`:

- **Clang args:** `-DWLR_USE_UNSTABLE` (still mandatory in 0.20), the wlroots
  include paths, `$OUT_DIR/protocol-include`, and include paths probed from
  `egl` plus whatever the enabled subsystems need (`glesv2`, `vulkan`,
  `libinput`, `xcb`). EGL is unconditional because `wlr/render/egl.h` is.
- **Allowlist:** `wlr_.*`, `WLR_.*` (types, functions, and vars)
- **Blocklist:** external types, re-imported per the type-interop table below
- **Enum style:** `newtype`, **not** `rust_non_exhaustive` as originally
  specified. The absence of C bitfields does not make a Rust `enum` safe here:
  wlroots has bitmask enums (`wlr_edges`, `wlr_output_state_field`) whose values
  routinely fall outside the declared variants, and materialising one of those as
  a Rust `enum` is UB. A newtype keeps the type distinction without the claim.
- **Layout assertions:** ON. bindgen 0.72 emits these as `const _` blocks, so
  they are checked at **compile** time on every build, not as `#[test]`s.
- **Rerun triggers:** `rerun-if-changed` on the wlroots include directory, the
  vendored protocol XML, and `build.rs`. The pkg-config env vars need no explicit
  `rerun-if-env-changed`: `pkg-config-rs` already emits them (verified — it
  emits `PKG_CONFIG_PATH` 10× and `PKG_CONFIG_SYSROOT_DIR` 15× per build, plus
  the target-suffixed variants).

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
| `xkb_keymap`, `xkb_state`, `xkb_context`, `xkb_mod_mask_t` | `xkbcommon-sys` | 1.4 |
| `libinput_device` and other `libinput_*` | `input-sys` (optional) | 1.19 |
| `size_t`, `timespec`, `dev_t`, `clockid_t` | `libc` | 0.2 |

The blocklist is an explicit name list, **not** a `wl_.*` regex: the core
protocol enums (`wl_output_transform`, `wl_seat_capability`, `wl_shm_format`, …)
are not provided by `wayland-sys` and must be generated locally.

`input-sys` is an **optional** dependency gated behind `libinput-backend`,
because it emits an unconditional `#[link(name = "input")]` and would otherwise
link libinput into compositors that never touch that backend.

`libseat` types appear only as opaque pointers in `backend/session.h`, so no
`libseat-sys` dependency is needed.

### Deviation: pixman and `drmModeModeInfo` are generated locally

Two type families are **not** blocklisted. In both cases a mismatched layout
would be silent memory corruption rather than a compile error, and no maintained
crate offers a type whose identity can be verified:

- **pixman.** `pixman-sys` is at 0.1.0 and unmaintained, while
  `pixman_region32_t` is embedded **by value** in `wlr_surface` and
  damage-tracking structs.
- **`drmModeModeInfo`.** This was specified as coming from `drm-sys`, but that
  crate generates only the kernel uAPI header and exposes `drm_mode_modeinfo`.
  wlroots uses libdrm's *userspace* struct from `<xf86drmMode.h>`. They are
  distinct types that happen to agree on layout; equating them would be a claim
  this crate cannot check. **The `drm-sys` dependency was therefore dropped
  entirely** — it had no other use.

bindgen already sees both sets of headers through wlroots' own `-I` flags.

**Accepted cost:** the safe `wlr` wrapper cannot pass a `pixman` crate region or
a `drm-sys` mode straight to wlroots without a cast. Revisit if `pixman-sys`
becomes actively maintained or a crate binds `xf86drmMode.h`.

## Hand-written support modules

### `src/signal.rs`

`wl_signal_init`, `wl_signal_add` and `wl_signal_get` are `static inline` in
`wayland-server-core.h` — **no symbol exists to link against**. wlroots' entire
event model is `wl_signal_add(&thing->events.destroy, &my_listener)`, so the
crate is unusable without them.

Correction to the original plan: `wayland-sys` **does** already reimplement those
three in Rust, in its `server::signal` module. `wlr-sys` re-exports them rather
than duplicating the work, so callers need one import.

`wl_signal_emit_mutable` — the emit function wlroots itself uses, and the only
one safe when a handler unlinks a listener — is genuinely missing: it is a real
exported symbol in libwayland-server 1.22+, but `wayland-sys` does not declare
it. `wlr-sys` declares it in an `extern` block.

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
           "gles2-renderer", "session", "color-management"]
```

Defaults match a stock distro build, so the common case works with no
configuration. Minimal consumers use `default-features = false`.

`vulkan-renderer` was **removed from the defaults**: `wlr/render/vulkan.h`
includes `<vulkan/vulkan_core.h>`, and the Vulkan headers are a separate package
(`vulkan-headers`, `libvulkan-dev`) that wlroots does not pull in. Leaving it on
by default would break the build on any machine with wlroots but not those
headers — including the development machine this was written on. Same reasoning
applies to `gles2-renderer` (`<GLES2/gl2.h>`), but mesa supplies those headers as
a wlroots dependency in practice, so it stays on.

## Error handling

| Condition | Behavior |
|---|---|
| wlroots not installed | Hard build error naming pkg-config package + distro packages |
| Wrong wlroots minor installed | Hard build error naming the matching `wlr-sys` version |
| Feature enabled, `have_*` false | `cargo::warning`, feature disabled |
| libclang missing | bindgen's own error; README documents the requirement |
| `wayland-scanner` missing | Build error naming the distro packages |

All three build-script paths were exercised against doctored `.pc` files and
produce the intended message.

## Testing

1. **bindgen layout assertions** — several hundred (648–688 depending on the
   feature set), emitted as `const _` blocks and therefore checked at **compile**
   time on every build (bindgen 0.72 no longer generates
   `__bindgen_test_layout_*` runtime tests). This is the primary safety net for
   *layout*: if `wayland-sys`'s `wl_list` layout ever diverged, every `wlr_*`
   struct embedding it fails to compile instead of corrupting memory.
2. **`tests/interop.rs`** — the safety net for type *identity*, which layout
   assertions cannot provide. A name missing from a blocklist makes bindgen
   generate a local duplicate that silently shadows the glob import, and
   everything still compiles; this is how `libinput_tablet_tool` escaped. The
   test coerces real wlroots functions to signatures written in the ecosystem
   crates' types, so a re-shadowing breaks the build.
3. **`tests/link.rs`** — asserts `wlr_version_get_major/minor/micro` agree with
   the `WLR_VERSION_*` constants bindgen lifted from the headers. Proves the
   linked `.so` matches the headers bindgen read, down to the patch version —
   wlroots offers no ABI guarantee across patch releases either.
4. **`tests/signal.rs`** — exercises the hand-written code that no generated
   assertion covers: signal delivery order across multiple listeners,
   `wl_signal_get`, `wl_list` iteration order, removal of the current element
   mid-iteration, and `container_of!` against `offset_of!`.
5. **`examples/headless.rs`** — `wl_display_create` → `wlr_headless_backend_create`
   → `wlr_backend_start` → one event-loop dispatch → teardown. End-to-end smoke
   test requiring no GPU and no seat.
6. **CI** — GitHub Actions on an Arch container (currently the only distro
   shipping wlroots 0.20): `fmt`, `clippy` (incl. `--features vulkan-renderer`),
   a `--no-default-features` build, `test`, `cargo doc` under
   `-D warnings` (the README *is* the rustdoc), a mixed-feature matrix that
   exercises the transitive `session.h` path, and running the example.

## Out of scope (YAGNI)

- Vendored meson/ninja build of wlroots
- `dlopen`-based loading
- Multi-version support (0.19 / 0.21 / auto-detection)
- Any safe abstraction in `wlr-sys`: no `Drop`, no lifetimes, and no trait that
  imposes a safety invariant — raw pointers only. (`wl_list_iter` implements
  `Iterator`, but its `Item` is `*mut T`, its only constructor is `unsafe`, and
  no safe handle escapes, so it adds no invariant.)
- Wayland protocol code generation *beyond* the two `wlr-protocols` server
  headers wlroots' own public headers require (see step 4). No Rust-side protocol
  bindings are generated.

## Accepted consequences

**docs.rs will not build this crate.** Bindings are generated at build time only,
with no checked-in fallback, and docs.rs has no wlroots installation and will not
run arbitrary package installs. `docs.rs/wlr-sys` will show a build failure, and
the same applies to the `wlr` wrapper unless it is `cfg`-guarded. This was chosen
knowingly over a committed-fallback `bindings.rs`; documentation is generated
locally with `cargo doc`.

**libclang and `wayland-scanner` are hard build dependencies** for every consumer
of the crate.

**Consumers must have wlroots 0.20 installed.** There is no fallback path.

## Deviations found during implementation

Each is explained in context above; collected here for review.

| # | Spec said | Reality | Resolution |
|---|---|---|---|
| 1 | Headers compile as shipped | Every header `#error`s without `-DWLR_USE_UNSTABLE` | Added to clang args |
| 2 | No protocol codegen needed | Two public headers include uninstalled `wlr-protocols` headers | Vendored the XML, run `wayland-scanner` at build time |
| 3 | `drmModeModeInfo` from `drm-sys` | `drm-sys` exposes only the kernel uAPI `drm_mode_modeinfo` | Generate locally; `drm-sys` dependency dropped |
| 4 | `input-sys` a plain dependency | It emits an unconditional `#[link(name = "input")]` | Made optional, gated on `libinput-backend` |
| 5 | Reimplement `wl_signal_*` in Rust | `wayland-sys` already has init/add/get | Re-export those; declare only `wl_signal_emit_mutable` |
| 6 | Enum style `rust_non_exhaustive` | wlroots has bitmask enums; a Rust `enum` would be UB | Use `newtype` |
| 7 | Layout tests run under `cargo test` | bindgen 0.72 emits `const _` assertions | Checked at compile time instead — strictly better |
| 8 | `vulkan-renderer` on by default | Vulkan headers are a separate package wlroots does not require | Moved out of `default`; covered explicitly in CI |
| 9 | `color-management` gates a header | `wlr/render/color.h` is always bindable | Feature retained, but emits only the `cfg` |

### Verification status

All nine deviations and every feature combination have been exercised locally,
including `--features vulkan-renderer` after installing the `vulkan-headers`
package: 123 headers bound instead of 122, 13 Vulkan symbols generated, and
`wlr_has_vulkan_renderer` emitted.
