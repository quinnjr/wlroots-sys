# wlr

Safe bindings to [wlroots](https://gitlab.freedesktop.org/wlroots/wlroots),
built on [`wlr-sys`](https://crates.io/crates/wlr-sys).

`wlr`'s minor version tracks the wlroots minor it binds, so pick the one
matching your system:

| `wlr` | wlroots | Packaged by |
|---|---|---|
| `0.20` | 0.20 | Arch |
| `0.19` | 0.19 | Ubuntu 26.04 |
| `0.17` | 0.17 | Ubuntu 24.04 |
| `0.15` | 0.15 | Ubuntu 22.04 |

```toml
wlr = "0.20"
```

The API is the same across all four, so moving between them is a version
change rather than a code change.
