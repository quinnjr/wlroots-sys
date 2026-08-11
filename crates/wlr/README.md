# wlr

Safe bindings to [wlroots](https://gitlab.freedesktop.org/wlroots/wlroots),
built on [`wlr-sys`](https://crates.io/crates/wlr-sys).

`wlr`'s minor version tracks the wlroots minor it binds, so pick the one
matching your system:

| `wlr` | wlroots | Packaged by | Status |
|---|---|---|---|
| `0.20` | 0.20 | Arch | published |
| `0.19` | 0.19 | Ubuntu 26.04 | planned |
| `0.17` | 0.17 | Ubuntu 24.04 | planned |
| `0.15` | 0.15 | Ubuntu 22.04 | this branch, unpublished |

```toml
wlr = "0.15"
```

`0.20` is the only line on crates.io today, so the snippet above will not
resolve yet — build against this branch by path until `0.15` is published.

The API is held identical across all four, so once they ship, moving between
them is a version change rather than a code change. Where wlroots itself
differs — this version commits an output's implicit pending state rather than an
explicit `wlr_output_state`, and creates a backend from a `wl_display` rather
than a `wl_event_loop` — the difference is absorbed inside the crate and none
of it reaches a public signature.
