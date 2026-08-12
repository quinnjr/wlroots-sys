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

## Compatibility policy, and its one recorded exception

Releases within a `wlr` minor line are **additive only**: no published item
changes name, signature, or trait bounds, and no published item changes what
it observably does. Everything from `0.20.0` onward has held to the first
half of that without exception — a full public-surface diff across
`0.20.4..0.20.11` shows no renamed, re-signed, or re-bounded item.

**`0.20.11` is a knowing exception to the second half.** The banded scene
tree (see that release's own section below) was necessary — layer-shell
stacking cannot be made correct by bookkeeping alone, and the alternative
was shipping a protocol whose ordering guarantee silently depended on
creation order — but it changed the observable behavior of two things that
already existed:

- **`raise_toplevel`** used to mean "above every sibling in the scene root".
  It now means "above every other toplevel", because toplevels are siblings
  of each other inside the toplevel band rather than siblings of everything.
  A `Top`/`Overlay` layer surface stays above a raised toplevel, which is
  the point.
- **An un-lowered root `add_rect`/`add_buffer` node** used to sit above
  whatever existed when it was created, with later toplevels appending above
  it. It now sits permanently above every toplevel and every layer surface,
  `Overlay` included, because it is a sibling of the fixed bands. It also
  swallows pointer hit-tests over its own area (a scene rect is not a
  surface, so a hit on it clears pointer focus instead of falling through) —
  documented on `add_rect`/`add_buffer` themselves.

Who this can bite: a `0.20.4`-era consumer that used an un-lowered root rect
as an interim decoration band, or that relied on `raise_toplevel` to lift a
window above such a rect. Upgrading to `0.20.11` regresses that silently —
it still compiles, it still runs, it draws in a different order. The
migration is `add_rect_in_toplevel` (`0.20.5`) for anything meant to move
with a window, `lower_rect_to_bottom` for anything meant to be a
background, and `add_rect_in_band` (`0.20.12`) for anything meant to place a
node *between* bands.

This is also the exception's own caveat, made explicit: within this line,
"documented" is standing in for "semver-compatible". A consumer pinning
`^0.20` in their `Cargo.toml` — the normal way to depend on this crate —
picks up the `raise_toplevel` and un-lowered-rect stacking change described
above on a plain `cargo update`, with no version bump of their own to
review against. The disclosure here is what makes that acceptable, not a
guarantee that nothing observable moved.

No further compatibility exception is sanctioned in the `0.20` line.

## 0.20.12

Scene-band placement and output assignment for layer surfaces; the two gaps
`0.20.11` deferred, both closed additively.

- `Band` and `Runtime::add_rect_in_band` — a solid-colour rect parented into
  a named scene band (`Background`/`Bottom`/`Toplevel`/`Top`/`Overlay`)
  rather than the scene root: it stacks *with* its band instead of sitting
  above everything, and (unlike `add_rect`) does not swallow pointer input
  over its area purely by virtue of being on top. `Band` is a new,
  five-variant enum, not `Layer` reused — `Layer` is the four-variant
  wire vocabulary a layer-shell client speaks, and `Band::Toplevel` (the
  band every toplevel's own tree lives in) has no client-facing
  counterpart. Removed by `Runtime::remove_rect`, exactly like a root rect;
  never purged by a toplevel's death, even for a `Band::Toplevel` rect —
  the toplevel band tree outlives every toplevel ever parented into it.
- `Runtime::set_layer_surface_output` — assigns a layer surface's output,
  discharging the responsibility wlr-layer-shell's own doc for
  `new_surface` states plainly: "the output may be NULL. In this case, it
  is your responsibility to assign an output before returning." Assigns the
  role object's `output` field directly; wlroots exposes no setter function
  for it. `None` on an unknown/stale layer-surface id or output id, the
  layer-surface id resolved first.
- `Runtime` now pins the `wl_display` `init_graphics` was given and
  `debug_assert_eq!`s it, in `add_rect`/`add_rect_in_band`/`commit_output`,
  against whichever `Display` the current `Backend::run_all` call is
  actually driving — a debug-only bug detector (compiled out under
  `--release`) for a `Runtime` clone driven by a `run_all` call for a
  different `Display` than the one it was initialized against. See
  `Runtime`'s own doc, "Lifetime obligation" section.

## 0.20.11

wlr-layer-shell.

- `Runtime::create_layer_shell` — the zwlr_layer_shell_v1 global.
- `Runtime::configure_layer_surface` / `Runtime::set_layer_surface_position`
  / `Runtime::focus_layer_keyboard` — id-keyed mutators for a layer
  surface's size, scene position, and keyboard focus.
- `LayerSurfaceId`, `LayerSurface`, `Layer`, `Anchor` — the borrow-scoped
  handle and its stable id, and the two small value types it exposes.
- `ToplevelHandler::new_layer_surface` / `layer_surface_commit` /
  `layer_surface_mapped` / `layer_surface_unmapped` /
  `layer_surface_destroyed` — additive, defaulted; the same "no `impl`
  written against an earlier 0.20.x has to change" guarantee every prior
  additive release here keeps.

Scene placement uses five fixed scene sub-trees ("bands"), created once at
`init_graphics` time and never reordered: `Background` < `Bottom` <
toplevels < `Top` < `Overlay`. A layer surface is created directly inside
its own band and reparented into a different one if a later commit reports
a different layer; every toplevel lives inside the toplevel band instead of
under the scene root. This makes the stacking order structural — a `Top`
panel stays above every toplevel regardless of creation order or any
`raise_toplevel` call — so there is no `raise_layer_surface` method, and
none is needed. See `Layer`'s own doc for the full design.

One observable delta from banding: a root-level rect/buffer added with
`add_rect`/`add_buffer` and never lowered now sits permanently above every
toplevel and every layer surface, `Overlay` included — because it is a
sibling of the (fixed) bands rather than a sibling of individual toplevels.
Previously a toplevel created after such a rect/buffer would append above
it. `lower_rect_to_bottom`/`lower_buffer_to_bottom` are unaffected either
way and remain the way to get a background.

**Answering `new_layer_surface`/`layer_surface_commit` with
`Runtime::configure_layer_surface` is mandatory, not optional.** Unlike
xdg-shell, nothing in this crate's dispatch layer sends a fallback
configure for a layer surface; a `ToplevelHandler` that ignores every layer
surface it is handed leaves that client permanently unmapped, waiting for a
configure that never comes. See `layer.rs`'s own module doc for the detail.

## 0.20.10

Output layout.

- `Runtime::output_layout_box` — an output's layout-coordinate box, by id.
- `Runtime::set_output_position` — pin an output at an explicit position.

## 0.20.9

xdg-decoration. Supersedes the yanked 0.20.8 — use this one.

- `Runtime::create_xdg_decoration_manager` / `Runtime::set_decoration_mode`
  — the zxdg_decoration_manager_v1 global and per-toplevel mode setting.
- `ToplevelHandler::request_decoration_mode` — additive, defaulted; the
  dispatch layer answers server-side when the handler stays silent.
- `DecorationMode` — `ClientSide` / `ServerSide`, spoken by both halves of
  the negotiation. The client's stated preference arrives as
  `Option<DecorationMode>` (`None` = stated nothing) and the answer takes a
  `DecorationMode`, so honoring the client is passing the value straight
  through. 0.20.8 used a `bool` on each side with *opposite* polarity and is
  yanked for it; see below.
- A decoration's mode is answered at the toplevel's *initial commit*, not
  when the client's request arrives: wlroots cannot be told a mode before
  the surface's first role commit initializes it, so `set_decoration_mode`
  stages the choice and the initial commit flushes it. Last write wins, and
  the staging is invisible to the caller. A toplevel whose client never
  states a preference still gets the handler its say at that commit, with
  `preference: None`; handler silence means server-side.
- A mode chosen from `ToplevelHandler::initial_commit` is now honored. In
  0.20.8 the surface was already initialized at that point, so the answer
  went out immediately and left nothing staged — which the "has this been
  answered?" check misread as "nobody has answered", and the server-side
  default overrode the compositor's choice one step later.
- A decoration created *after* its toplevel's initial commit is now
  answered. That ordering is legal (the protocol only forbids
  `get_toplevel_decoration` once a buffer is attached), and in 0.20.8 such a
  decoration whose client never called `set_mode` was never answered at all,
  so the client waited forever for a configure it never received.
- `Runtime::configure_toplevel` (0.20.7) is now covered by the same
  pre-initialization guard: called before its toplevel's initial commit it
  is a no-op returning `Some(())`, rather than tripping wlroots'
  `surface->initialized` assertion. Nothing is lost — the initial commit
  schedules a configure of its own, carrying any state that was staged.
  Behavior hardening of a documented contract, not a semantic break.

## 0.20.8 — yanked

Never use this version. Its decoration API took a `bool` on both sides of
the negotiation, and the two had opposite polarity: the handler's `true`
meant client-side while the mutator's `true` meant server-side, so the
natural "honor what the client asked for" implementation passed the value
through and silently did the opposite. It compiled without warning. Caught
by review after publication and yanked with no dependents, which is why
0.20.9 is free to replace the surface rather than deprecate around it.
0.20.9 is otherwise the same feature, plus two negotiation fixes.

## 0.20.7

Client-driven state requests.

- `ToplevelHandler::{request_maximize, request_fullscreen, request_move,
  request_resize}` — fully-defaulted, additive. The dispatch layer always
  schedules the protocol-required configure answer for state requests, so
  ignoring a request stays protocol-correct.
- `Edges` — resize-edge flags.
- `Runtime::configure_toplevel` — a bare configure by id.

## 0.20.6

Pixel-buffer scene nodes.

- `BufferId` and the `Runtime::*_buffer` family — scene nodes displaying
  owned RGBA8888 pixels (copied in), at the scene root or inside a
  toplevel's tree; reposition, rescale (`set_buffer_dest_size`), replace
  pixels, lower, remove — all by id, all `Option` on stale ids.
- `add_buffer` errors (`Error::Create`) if `init_graphics` has not run yet,
  symmetric with `add_rect`.

## 0.20.5

Milestone-1 residuals.

- `Runtime::add_rect_in_toplevel` / `Runtime::remove_rect` — rects that live
  inside a toplevel's scene tree and move/raise/die with it; rect removal
  by id for root rects too.
- `Runtime::remove_fd` — fd sources removable by id, live or declared.
- `wl_event_loop_dispatch` is retried on EINTR instead of failing the run.
- `KeyEvent::for_test`, `Toplevel::current_size`, `Toplevel: Debug` — test
  and introspection surface for consumers.
- `Backend::run`'s docs now steer real compositors to `run_all`.

## 0.20.4

Seat, keyboard and pointer input.

- `SeatHandler` gains `key`, `pointer_motion` and `pointer_button`. All
  defaulted, so an empty impl written against 0.20.1 keeps compiling.
- `KeyEvent<'h>` and `Modifiers` — a key's layout-agnostic, unshifted keysym
  and modifier state.
- `Runtime::create_seat`, `focus_toplevel_keyboard`, `clear_keyboard_focus`,
  `toplevel_at` (the scene hit test — the window under a point, and the
  point's position *within that window*, in the same coordinates
  `set_toplevel_position` uses) and `pointer_position`.

`SeatHandler::key` returns `true` to consume a key and keep it from the
focused client. Consumption is best-effort by design: a key that arrives
while another handler is running is forwarded before this can answer, so a
binding must not depend on the client never seeing the key.

`focus_toplevel_keyboard` reports a miss (`None`) for an **unmapped**
toplevel, the same as for an unknown id — there is no unmapped-surface
concept elsewhere in this crate's model, so this is the one place a
by-id caller could otherwise have asked wlroots to focus a surface no
client is rendering to.

**Bug fix, on a frozen surface:** `ToplevelId::dangling_nth_for_test(n)`'s
*value* changes for `n >= 2^32` (previously `u64::MAX - n`, unclamped; now
folded into a fixed 2^32-wide band via `u64::MAX - (n % 2^32)`). `n = 0` and
every `n` this crate's own tests or any reasonable caller would pass
(`n <= 8` is the documented range) are bit-for-bit unaffected — only a caller
already passing `n` in the billions, which no known caller does, would see a
different id than before. Fixed because the unclamped subtraction could wrap
into real-id space for a large enough `n`, contradicting the "no live
toplevel can have this id" guarantee `dangling_nth_for_test`'s own doc makes.

## 0.20.3

Testing utility, no protocol surface.

- `ToplevelId::dangling_nth_for_test(n)`: like `dangling_for_test`, an id no
  live toplevel can ever have, but distinguishable by `n` -- a consumer
  driving handler logic for more than one toplevel without a real client
  needs ids that compare unequal to each other, which a single fixed
  dangling value can't give it. `n = 0` collides with `dangling_for_test`'s
  value on purpose (both sit at the top of the same reserved id band); pass
  `n >= 1` for an id distinct from that one too.

The seat (`SeatHandler`'s methods) is still 0.20.1's empty trait; it did not
land in this release and remains planned for a later 0.20.x.

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
  methods arrive additively in later 0.20.x releases (`ToplevelHandler`'s in
  0.20.2; `SeatHandler`'s are still pending).
- `RectId` and the `Runtime::*_rect` family — solid-colour scene nodes.
- `Output::size`, `Output::schedule_frame`,
  `Output::enable_with_preferred_mode`, `Display::add_socket_auto`,
  `Display::flush_clients`.

`Backend::run` is unchanged and stays forever as the output-only path.
