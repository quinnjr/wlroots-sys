# wlr

Safe bindings to [wlroots](https://gitlab.freedesktop.org/wlroots/wlroots),
built on [`wlr-sys`](https://crates.io/crates/wlr-sys).

`wlr`'s minor version tracks the wlroots minor it binds, so pick the one
matching your system:

| `wlr` | wlroots | Packaged by | Status |
|---|---|---|---|
| `0.20` | 0.20 | Arch | published |
| `0.19` | 0.19 | Ubuntu 26.04 | planned |
| `0.17` | 0.17 | Ubuntu 24.04 | this branch |
| `0.15` | 0.15 | Ubuntu 22.04 | planned |

```toml
wlr = "0.17"
```

This is the `0.17` line, for Ubuntu 24.04's wlroots. `0.20` is the line that is
on crates.io today; the remaining lines are named here because
[`wlr-sys`](https://crates.io/crates/wlr-sys) already binds them and `wlr`
follows on the same branches, not because they are all published yet.

The API is held identical across all four, so moving between them is a version
change rather than a code change. Where wlroots itself differs — 0.17's
`wlr_backend_autocreate` takes a `wl_display` where 0.20's takes a
`wl_event_loop`, for one — the difference is absorbed inside this crate, not
passed on to you.
