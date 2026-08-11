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
| `0.15` | 0.15 | Ubuntu 22.04 | planned |

```toml
wlr = "0.20"
```

Only `0.20` exists today. The older lines are named here because
[`wlr-sys`](https://crates.io/crates/wlr-sys) already binds them and `wlr`
follows on the same branches, not because they are on crates.io yet.

The API is held identical across all four, so once they ship, moving between
them is a version change rather than a code change.

## 0.20.2

xdg-shell.

- `ToplevelHandler` gains `new_toplevel`, `initial_commit`, `mapped`,
  `unmapped`, `title_changed` and `toplevel_destroyed`. All defaulted, so an
  empty impl written against 0.20.1 keeps compiling.
- `Toplevel<'h>` and `ToplevelId`: borrow-scoped handle, storable id.
- `Runtime::create_xdg_shell`, and the by-id operations —
  `set_toplevel_size`, `set_toplevel_activated`, `set_toplevel_maximized`,
  `set_toplevel_fullscreen`, `set_toplevel_position`, `set_toplevel_visible`,
  `raise_toplevel`, `close_toplevel`.

Toplevels are inserted into the scene graph by the library; the consumer
positions them by id.

Two ordering rules are worth reading before you write against this:
`create_xdg_shell` requires `init_graphics` to have run (it returns
`Error::Operation` rather than leaving you with clients that never map), and
a `ToplevelId` resolves only for the `Backend::run_all` call that announced
it — every by-id operation returns `None` for one held past that point.

## 0.20.1

Event sources and the scene graph.

- `Runtime` — the long-lived handle a compositor keeps: fd sources, the scene
  graph, the output layout, the renderer, and every by-id operation.
- `Interest`, `Readiness`, `SourceId`, `FdHandler` — file-descriptor sources,
  registered by each `run_all` and re-armed by the next one.
- `LoopHandler`, `Until`, `Backend::run_all` — a blocking entry point that
  stops when the consumer says so, and flushes clients every turn.
- `ToplevelHandler` and `SeatHandler` are declared here with no methods, so
  that `Handlers`' supertrait list is fixed from the first release; their
  methods arrive in 0.20.2 and 0.20.3 and are additive.
- `RectId` and the `Runtime::*_rect` family — solid-colour scene nodes.
- `Output::size`, `Output::schedule_frame`,
  `Output::enable_with_preferred_mode`, `Display::add_socket_auto`,
  `Display::flush_clients`.

`Backend::run` is unchanged and stays forever as the output-only path.
