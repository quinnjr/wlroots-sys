# Vendored protocol XML

These files are copied verbatim from
[wlr-protocols](https://gitlab.freedesktop.org/wlroots/wlr-protocols).

They are vendored because two of wlroots' *installed* public headers —
`wlr/types/wlr_layer_shell_v1.h` and `wlr/types/wlr_output_power_management_v1.h`
— `#include` the generated `-protocol.h` counterparts, but wlroots keeps those
generated headers private to its own build and never installs them. `build.rs`
regenerates them into `OUT_DIR` with `wayland-scanner server-header`.

They are not inert: 32 `zwlr_*` / `ZWLR_*` items reach the generated bindings,
and `wlr_layer_surface_v1`'s fields are typed with them. A silent change here is
a silent change to this crate's public API and to the wire protocol a compositor
speaks.

## Pinned upstream revision

| Field | Value |
|---|---|
| Repository | `https://gitlab.freedesktop.org/wlroots/wlr-protocols` |
| Branch | `master` |
| Commit | `bf4fc79abc359eea5a0edec0ac6d4a2b2955f82a` |
| Retrieved | 2026-08-03 |

## Checksums

```
87e0b9c837aecd6977f76f3c47d73088b7159871f5d979dc1840f6cadb5e2ed8  wlr-layer-shell-unstable-v1.xml
7ebd98f3449d246a57829e4b4dd9fbc3ef98e3dd42fa94ea102f14f490eb20de  wlr-output-power-management-unstable-v1.xml
```

Verify locally:

```sh
cd crates/wlr-sys/protocol && sha256sum -c SHA256SUMS
```

CI runs that check, so any change to these files is a deliberate, reviewable
edit to `SHA256SUMS` rather than an invisible byte change inside a 19 KB XML
file that no reviewer diffs by eye.

## Updating

Do this only alongside a wlroots bump — the vendored XML must agree with the
`wlr-protocols` revision the installed wlroots was built against, or the crate
generates one enum set against a library expecting another.

1. Fetch the new files from the wlroots-pinned `wlr-protocols` revision.
2. Update the commit SHA and retrieval date above.
3. Regenerate `SHA256SUMS` (`sha256sum *.xml > SHA256SUMS`).
4. Rebuild and check `cargo test -p wlr-sys` — `tests/interop.rs` will catch a
   type-identity change, but an enum *value* change is silent, so diff the
   generated `zwlr_*` constants if the upstream diff touched any `<entry>`.

## Licensing

Each XML carries its own MIT-style copyright notice in-file; those govern these
two files, not this crate's `LICENSE`.
