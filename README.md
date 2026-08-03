# wlroots-sys

Rust bindings to [wlroots](https://gitlab.freedesktop.org/wlroots/wlroots).

| Crate | Description |
|---|---|
| [`crates/wlr-sys`](crates/wlr-sys) | Raw FFI bindings to wlroots 0.19 |

A safe wrapper crate (`wlr`) will be added here as a second workspace member.
Keeping both in one workspace means the wrapper cannot drift from the bindings it
wraps — which matters, because wlroots breaks its API every minor release.

## Versioning

Each crate's minor version tracks the wlroots minor version — see
[`crates/wlr-sys/README.md`](crates/wlr-sys/README.md#versioning).

## Quick start

```sh
# Requires wlroots 0.19 + headers, libclang, and wayland-scanner.
cargo test --workspace
cargo run -p wlr-sys --example headless
```

See [`crates/wlr-sys/README.md`](crates/wlr-sys/README.md) for requirements,
feature flags, and how the crate interoperates with the wayland-rs ecosystem.

## Design

[`docs/superpowers/specs/2026-07-29-wlr-sys-design.md`](docs/superpowers/specs/2026-07-29-wlr-sys-design.md)

## License

MIT.
