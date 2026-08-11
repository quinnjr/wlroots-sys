# wlroots-sys

Rust bindings to [wlroots](https://gitlab.freedesktop.org/wlroots/wlroots).

| Crate | Description |
|---|---|
| [`crates/wlr-sys`](crates/wlr-sys) | Raw FFI bindings to wlroots 0.15 |
| [`crates/wlr`](crates/wlr) | Safe bindings, built on `wlr-sys` |

Both live in one workspace so the wrapper cannot drift from the bindings it
wraps — which matters, because wlroots breaks its API every minor release.

`wlr-sys` is raw: pointers, pervasive `unsafe`, no lifetimes. `wlr` is where the
ownership problem is solved — wlroots frees objects whenever it likes, announced
by a `destroy` signal, so `wlr` hands handlers borrow-scoped handles that cannot
outlive the call they were passed to, and keys long-lived state by an id that
self-cleans when the object dies.

## Versioning

Each crate's minor version tracks the wlroots minor version — see
[`crates/wlr-sys/README.md`](crates/wlr-sys/README.md#versioning).

## Quick start

```sh
# Requires wlroots 0.15 + headers, libclang, and wayland-scanner.
cargo test --workspace
cargo run -p wlr-sys --example headless
```

See [`crates/wlr-sys/README.md`](crates/wlr-sys/README.md) for requirements,
feature flags, and how the crate interoperates with the wayland-rs ecosystem.

## Design

[`docs/superpowers/specs/2026-07-29-wlr-sys-design.md`](docs/superpowers/specs/2026-07-29-wlr-sys-design.md)

## License

MIT.
