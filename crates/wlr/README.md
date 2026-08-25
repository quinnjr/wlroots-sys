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

## 0.20.21

Pointer constraints and relative pointer motion. All additive.

- `Runtime::create_pointer_constraints_manager(&Display) -> Result<()>` — the
  `zwp_pointer_constraints_v1` global. A client can **lock** the pointer (the
  cursor is frozen in place — for FPS games and 3D viewports) or **confine** it
  (the cursor is clamped to a region of a surface). The crate drives the whole
  enforcement in the pointer path: a constraint activates when its surface holds
  the seat's pointer focus (and deactivates on leave, warping to the client's
  `cursor_hint` on lock release); while locked, the cursor does not move; while
  confined, motion is clamped into the region. When a client moves the confine
  region off the current cursor (via `set_region`), the cursor is re-anchored
  into the new region rather than wedging — so a client cannot freeze its own
  pointer.
- `Runtime::create_relative_pointer_manager(&Display) -> Result<()>` — the
  `zwp_relative_pointer_manager_v1` global. Unaccelerated relative motion
  deltas are delivered to bound clients on every pointer motion, which is what
  makes a locked pointer usable (the cursor is frozen, but the client still
  receives the deltas).
- `Runtime::cursor_position(&self) -> (f64, f64)` — the cursor's layout
  position (a thin alias of `pointer_position`), for observing the cursor.

Errors if either `create_*` is called twice. No existing item's name or
signature changed.

## 0.20.22 — output management, M5 render & scene foundations

Two milestones ship together (0.20.21 predates both). All additive.

### Output management (`zwlr_output_management_v1`)

A compositor can now advertise output management, so any standard tool
(`wlr-randr`, `kanshi`, `wdisplays`, `nwg-displays`, or a bespoke settings app)
can enumerate output heads and request an atomic reconfiguration — resolution,
position, scale, transform, enabled/disabled.

- `Output::modes(&self) -> Vec<Mode>` and the new `Mode { width, height,
  refresh_mhz, preferred }` — enumerate an output's advertised modes.
- `Output::set_mode(width, height, refresh_mhz)`, `set_scale(f32)`,
  `set_transform(Transform)`, `disable()` — output-state setters, each built and
  committed with the same `wlr_output_state` scaffold as
  `enable_with_preferred_mode`.
- `Runtime::create_output_manager(&Display) -> Result<()>` — the
  `zwlr_output_manager_v1` global. `apply`/`test` requests are handled
  synchronously: every head is validated (`wlr_output_test_state`) before any is
  committed, then either all commit and the client is told `succeeded`, or none
  is changed and it is told `failed`; the configuration is destroyed exactly
  once. Position, which `wlr_output_head_v1_state_apply` does not apply, is set
  manually. Errors if called twice.
- `OutputHandler::output_configuration_applied(&mut self, Vec<AppliedHead>)` — a
  defaulted, additive notification carrying **owned** `AppliedHead` data (no
  borrowed protocol object escapes the handler), the compositor's cue to
  re-derive geometry and persist the layout.
- `Runtime::update_output_manager_state(&self)` — re-advertise head state after
  an output is added, removed, or reconfigured.
- `Runtime::schedule_frame(id: OutputId) -> Option<()>` — schedule a frame for an
  output by id (the handle-free counterpart of `Output::schedule_frame`, for a
  re-enabled output whose only identity is its id).

Also fixes a **latent teardown use-after-free** affecting every seat-scoped
manager global (data-control, virtual-keyboard, relative/locked/confined
pointer): `Display::drop` now calls `wl_display_destroy_clients` before
`wl_display_destroy`, so client device resources are torn down while their
manager globals are still alive — matching what every wlroots compositor does.

### Geometry, transforms and regions

- `Box2D` and `FBox` — `wlr_box` and `wlr_fbox`, with the layout pinned to
  wlroots' at compile time so passing one across is a pointer cast. Predicates
  (`empty`, `contains_point`, `contains_box`, `closest_point`, `intersection`,
  `transformed`, `equals`) are FFI calls rather than reimplementations, so the
  edge cases cannot drift: containment is half-open, `contains_box` is false
  when *either* box is empty, and `closest_point` reports `None` where wlroots
  writes NaN. `From`/`Into` for `(x, y, width, height)` tuples, which is what
  the older signatures such as `Runtime::output_layout_box` use.
- `Transform` — the eight `wl_output_transform` values, with `invert`,
  `compose` (documented as non-commutative once a flip is involved),
  `apply_coords`, and lossless conversion to and from
  `wlr-sys`'s `wl_output_transform`, so a `wayland-server` consumer is never
  forced to launder a transform through this crate's type.
  **`apply_coords` is not a point transform** despite its C name: it swaps the
  axes for the four transforms that turn them and does nothing otherwise. Its
  doc says so; `Box2D::transformed` is the real one.
- `Region` and `RegionRef<'_>` — owned and borrowed pixel regions, covering
  both pixman's set operations (`union`, `intersect`, `subtract`, `translated`,
  `rectangles`, `extents`, `contains_point`) and wlroots' own helpers
  (`scaled`, `scaled_xy`, `transformed`, `expanded`, `rotated_bounds`,
  `confine`). No public API takes or returns a raw `pixman_region32`.
  `expanded` takes a `u32`, making wlroots' non-negativity precondition
  unrepresentable; `rotated_bounds` takes **radians**, which the headers do not
  say and which `tests/region.rs` pins.

### Rendering

The one part of wlroots a compositor genuinely *owns*: a renderer, an
allocator, a swapchain, a texture and a render pass are all created by the
compositor and freed by the compositor. Verified rather than assumed — the only
`wlr_renderer_destroy`/`wlr_allocator_destroy` calls inside wlroots 0.20.2 are
for the DRM backend's own internal renderer — so these types have real `Drop`
impls, and there is no "was it destroyed behind my back?" error among their
failures.

- `Renderer` — `autocreate` (whatever wlroots picks for a backend) or `pixman`
  (software, no GPU, no backend needed). Reports `features()`, `buffer_caps()`,
  `texture_formats()` and a **borrowed** `drm_fd()`; registers the buffer
  factory globals with `init_wl_display`/`init_wl_shm`; makes textures, render
  timers and render passes. `is_lost()` latches when the GPU is reset, and a
  compositor driven by `Backend::run_all` hears about the runtime's own renderer
  through the new `OutputHandler::renderer_lost` instead.
- `RendererRef<'_>` and `AllocatorRef<'_>`, from `Runtime::renderer_ref()` and
  `Runtime::allocator_ref()` — **non-owning** views of the pair
  `Runtime::init_graphics` creates and deliberately never frees. They carry the
  same queries with no `Drop` and no way to acquire one, because handing out an
  owning type for those would put a double free one `drop` away.
- `Texture<'r>` — borrows the renderer that made it, which is how wlroots'
  "textures must be destroyed separately" stops being a rule to remember and
  becomes a compile error (`tests/ui/texture_outlives_renderer.rs`).
  `update_from_buffer` documents its own **routine** failure: the pixman
  renderer implements no update path at all, and callers must be prepared to
  rebuild the texture instead. `read_pixels` bounds-checks the destination
  slice, which wlroots does not.
- `RenderPass<'r, 'b>`, with `TextureOptions`/`RectOptions` builders,
  `RenderColor`, `BlendMode` and `FilterMode`. **Dropping a pass submits it**:
  `wlr_render_pass_submit` is the only thing that frees a pass, wlroots has no
  cancel, and a forgotten pass leaks GPU memory. `submit()` is the same call
  with the answer returned. One live pass per renderer; a second returns
  `Error::Reentrant`.
- `Allocator<'r>` and `Swapchain<'a>`, with `SWAPCHAIN_CAP`. A swapchain whose
  allocator died reports `Error::Destroyed("wlr_allocator")` from `acquire`
  rather than calling into it — wlroots nulls the field from its own listener,
  so that is a fact rather than a cached guess.
- **`OwnedBuffer` and `LockedBuffer` are not interchangeable.** An allocator
  hands out the *producer* reference (released with `wlr_buffer_drop`); a
  swapchain hands out the *consumer* reference (released with
  `wlr_buffer_unlock`). Mixing them up is a leak in one direction and a
  premature free in the other, and wlroots diagnoses neither — so they are
  different types that both deref to the read-only `Buffer<'_>`.
- `DrmFormat`, `DrmFormatSet` (+ the `Ref` views), `FourCc` and `Modifier`.
  `DrmFormatSet::intersect` returns `Err` for an **empty** intersection as well
  as for a failure, because `wlr_drm_format_set_intersect` reports both as
  `false` and gives nothing to tell them apart; guessing would be wrong half the
  time.
- `DmabufAttributes` / `DmabufAttributesRef<'_>` — owned (closes its
  descriptors) and borrowed (closes none, because `Buffer::dmabuf`'s descriptors
  belong to the buffer). `try_clone` dups. `plane_fd` dups too, rather than
  handing out a descriptor the attributes still own.

Four wlroots assertions are reachable from an obvious safe call here, and every
distro builds wlroots without `NDEBUG`, so tripping one **aborts the process**.
They are checked in Rust instead: a renderer's own listeners are unlinked before
it is destroyed, `texture_from_pixels` rejects a zero dimension or a short
slice, `add_rect` rejects a negative extent, `add_texture` rejects a source box
outside its texture (and a texture belonging to another renderer), and
`Swapchain::create` rejects a non-positive size.

One implementation note worth knowing: `texture_from_pixels` keeps its own copy
of the pixels. wlroots wraps the caller's pointer in a buffer it immediately
drops, and the pixman renderer goes on reading through that *original* pointer —
so a `&[u8]` argument cannot promise what the texture needs.

### Colour

`wlr/render/color.h` in full: the vocabulary the render API speaks, and the
transforms a colour-managed compositor applies. Wrapped here rather than with
the colour-management protocols later, because a render pass cannot be described
without it.

- `NamedPrimaries`, `TransferFunction`, `ColorEncoding`, `ColorRange`,
  `ChromaLocation` and `AlphaMode`, plus the set types `ColorEncodings` and
  `TransferFunctions`. **Three of those C enums are bitmask-valued and three are
  sequential**, and nothing in the Rust shows which — `ColorEncoding::Bt709` is
  2 while `ColorRange::Limited` is 1. `tests/render_color.rs` pins every value
  against wlroots' own constants. The same enum is a *set* on the renderer
  (`Renderer::color_encodings`) and a *single value* on a texture, which is why
  there are two types rather than one.
  `TransferFunction` deliberately has no `Default`: its variants start at
  `1 << 0`, so the "unset" wlroots reads out of a zeroed struct is not a member
  of the enum, and this crate spells it `Option<TransferFunction>`.
- `Cie1931Xy`, `ColorPrimaries` and `ColorLuminances` — `#[repr(C)]` twins with
  their layouts pinned to wlroots'. `ColorPrimaries::named` fills one from a
  well-known volume; `transform_absolute_colorimetric` computes the 3×3
  conversion between two, and refuses a degenerate one rather than letting
  wlroots invert a singular matrix.
- `ColorTransform` — immutable and reference-counted, so `Clone` is a genuine
  second reference and one may outlive every renderer that ever applied it.
  Built from an ICC profile (where wlroots was compiled with lcms2), an inverse
  EOTF, three 1-D lookup tables, a matrix, or a pipeline of other transforms.
  **The matrix is row-major**, verified against `multiply_matrix_vector` in
  wlroots' `render/color.c` rather than guessed from a header that says only
  "a 3×3 matrix" — and it is the same order `transform_absolute_colorimetric`
  produces, so the two compose.
- Three more wlroots assertions are checked in Rust: an empty transform pipeline
  (`init_pipeline` asserts a non-zero length), mismatched or empty lookup
  tables (`init_lut_3x1d` reads `dim` entries from all three pointers whatever
  their real lengths, and a `dim` of 0 makes the evaluator index at `SIZE_MAX`),
  and a degenerate colour volume (`matrix_invert` asserts `det != 0`, and
  `ColorPrimaries::default()` is all zeroes — that one aborted the test binary
  before the check existed).
- The colour setters on `TextureOptions` and `BufferPassOptions` **fail** rather
  than being ignored when the renderer cannot honour them. wlroots' own answer
  is to draw anyway, untagged, which turns a colour-managed compositor into a
  quietly mis-rendering one.

### Explicit synchronisation

- `SyncTimeline` — a reference-counted DRM sync-object timeline, with
  `create`/`import`, `signal`, `check`, `transfer`, `export`, and the sync-file
  pair. `check` collapses wlroots' two-level answer the only way that keeps
  both: `Err` is "the ioctl failed", `Ok(false)` is "not ready yet".
- `SyncFlags::WAIT_FOR_SUBMIT` and `WAIT_AVAILABLE`. These are libdrm's
  `DRM_SYNCOBJ_WAIT_FLAGS_*`, not wlroots symbols, so they are not in `wlr-sys`
  and this crate writes them out — and they are **not** bits 0 and 1, because
  `WAIT_ALL` holds bit 0. `tests/render_sync.rs` reads the installed `drm.h` and
  compares.
- `EventLoop::wait_for_timeline` returns a `SyncWaiter<'_>`: a one-shot wait
  registered on the loop, cancelled by dropping it. `wlr_..._waiter_finish` is
  legal *and required* after the callback has fired — the callback path releases
  nothing — which was verified in wlroots' source rather than inferred from the
  header's word "cancel", since the two readings differ by a leak in one
  direction and a double free in the other. The waiter borrows the loop, so
  outliving the display is a compile error.
- The timeline's DRM descriptor must outlive the timeline: wlroots keeps it and
  uses it to destroy the kernel object. That one is documented rather than
  borrow-checked, on purpose — a lifetime there would make storing a timeline
  beside the renderer it came from a self-referential struct, and wlroots'
  whole reason for reference-counting timelines is that they are stored.

### Backend-specific renderer surfaces

Every `wlr_gles2_*`, `wlr_vk_*` and `wlr_pixman_*` accessor is undefined
behaviour on an object of the wrong kind, and each ships a separate
`wlr_*_is_*` test the caller is trusted to have run. Here the test produces a
value instead: `Renderer::as_pixman`, `as_gles2` and `as_vulkan` answer `Some`
only when it passed, and every backend-specific call hangs off the view. The
precondition is unrepresentable rather than documented.

- `Pixman<'_>`, `Gles2<'_>` and `Vk<'_>`, plus `Texture::pixman_image`,
  `gles2_attribs`, `vulkan_attribs` and `vulkan_has_alpha`, and
  `RenderTimer::is_gles2` (there is no `wlr_render_timer_is_vk` to mirror).
- `Egl`, for a compositor that initialises EGL itself, with
  `Renderer::gles2_from_egl` **consuming** it — that is wlroots' only release
  path, since there is no `wlr_egl_destroy`, so an `Egl` that never reaches a
  renderer leaks. Stated rather than papered over.
- GL names, Vulkan handles, `EGLDisplay` and `pixman_image_t` cross as whatever
  `wlr-sys` generated, from `unsafe` functions, with no `gl`/`ash`/`pixman`
  dependency and no normalisation. This crate wraps wlroots' *use* of those
  libraries; re-typing a handle would put two definitions of it in one process.

### The scene graph

Before M5 the only thing a consumer could put in the scene was a rect or a
pixel buffer, at the root, in a band, or in a toplevel. Now the graph itself is
addressable: trees, nesting, restacking, reparenting and hit testing, all by id.

- `NodeId` — the storable identity of a scene node, and the first id in this
  crate that is **addon-backed for its whole family**. `wlr_scene_node_destroy`
  frees the node *and every descendant* with no announcement; a payload on each
  node's own `wlr_addon_set` is what tells this crate, because wlroots runs an
  addon's destructor for every node it frees, cascade included. So a `NodeId`
  goes stale at exactly the right instant and every call on it misses cleanly
  from then on — see `tests/scene_destroy.rs`, which asserts that for a
  three-deep tree through every entry point.
- `NodeKind` and the borrow-scoped handles `SceneNode<'_>`, `SceneTree<'_>`,
  `SceneRect<'_>`, `SceneBuffer<'_>`, reached through `Runtime::with_node`,
  `with_tree`, `with_rect` and `with_scene_buffer`. Handles are read-only
  observation surfaces; every mutation is a by-id call on `Runtime`. The
  downcasts (`try_as_tree`/`try_as_rect`/`try_as_buffer`) check wlroots' type
  tag first — the C helpers are bare pointer casts and undefined behaviour on a
  mismatch.
- While a handle borrow is live, `destroy_node`, `reparent_node`, `remove_rect`
  and `remove_buffer` all return `None` without acting. A closure that freed
  the node it was just handed would be left holding a dangling handle, and a
  documented rule is weaker than a check.
- Structure: `create_tree_in_band`, `create_tree_under`, `create_rect`,
  `create_scene_buffer`, `destroy_node`, `reparent_node`, `set_node_enabled`,
  `set_node_position`, `place_node_above`, `place_node_below`,
  `raise_node_to_top`, `lower_node_to_bottom`.
- Queries: `node_kind`, `node_position`, `node_coords`, `node_enabled`,
  `node_parent`, `node_children`, `node_at`, `for_each_buffer`,
  `scene_root_node`, `band_node`, `rect_node`, `buffer_node`.
- Buffer nodes: `set_scene_buffer` (with `SceneBufferOptions` carrying a damage
  region and an explicit-sync wait point), plus `set_scene_buffer_dest_size`,
  `_source_box`, `_opaque_region`, `_transform`, `_opacity`, `_filter`,
  `_transfer_function`, `_primaries`, `_color_encoding` and `_color_range`.
- **Every wlroots `assert()` reachable from here is a `None` instead.** Arch
  ships wlroots with assertions enabled, so an unchecked call would abort the
  process rather than fail: `place_node_above`/`_below` refuse a node placed
  against itself and a pair with different parents, `reparent_node` refuses a
  cycle, the size setters refuse a negative dimension. `tests/scene_tree.rs`
  exercises each one, and the process surviving that test *is* the assertion.
- Three origins, because not every node in the scene is the consumer's to
  restructure. Nodes this crate created for them are fully mutable; the scene
  root and the six bands are readable and enable-able but never destroyed,
  restacked or reparented; and a node wlroots owns — a toplevel's tree, a layer
  surface's tree, a client's surface node — gets an id when `node_at` or
  `node_children` reaches it, but only reads and property changes. Restack
  those through `raise_toplevel` and its siblings, which keep this crate's own
  placement bookkeeping straight.
- `RectId` and `BufferId` are unchanged and still work exactly as before, but
  the nodes under them now carry the same payload: `rect_node`/`buffer_node`
  bridge to the node API, a cascade that frees such a node drops its row (so
  `remove_rect` afterwards misses instead of double-destroying), and
  `reparent_node` recomputes the parent tracking those two tables still use.

### Cargo features

`wlr` now re-exports `wlr-sys`' feature names one for one — `drm-backend`,
`x11-backend`, `libinput-backend`, `session`, `gles2-renderer`,
`vulkan-renderer`, `xwayland` — with the same default set, and forwards them
rather than letting `wlr-sys` pick its own. They decide which wlroots headers
are bound, and so which of the `wlr_has_*` cfgs are set: building without
`gles2-renderer` removes `Renderer::as_gles2`, `Gles2` and
`RenderTimer::is_gles2`, because the symbols behind them are no longer there.

### Logging

- `LogLevel`, `init_logging` and `log_verbosity`. `init_logging` installs a
  process-global sink (wlroots' callback has no user-data pointer, so there can
  only be one) and sets the verbosity.
- Two things worth knowing before you install one. **wlroots does not apply the
  verbosity filter to a custom callback** — that test lives inside its own
  stderr logger — so this crate applies it in the trampoline, and `level` means
  the same thing either way. And a **panic escaping the sink is caught and
  discarded**: a logger that aborts the process is worse than one that loses a
  line. Everywhere else in this crate a panic aborts, deliberately; this is the
  one exception.
- Lines longer than 4 KiB are truncated. Rust cannot read a `va_list`, so the
  formatting goes back through C's `vsnprintf` into a fixed buffer, and
  two-pass sizing would need `va_copy`, which stable Rust also lacks.

### Internal

- The `wlr_addon` code that backs `OutputId`/`ToplevelId`/`LayerSurfaceId` is
  now a reusable substrate rather than one hand-rolled copy in `id.rs`. No
  public API change; the existing suite passing unchanged is the acceptance
  criterion.

## 0.20.23 — XWayland

An additive XWayland wrapper: a compositor can host X11 clients as
first-class windows, place and decorate them, and handle override-redirect
pop-ups. Gated on the `xwayland` feature (on by default) and the
`wlr_has_xwayland` cfg, so a build without the wlroots xwayland headers is
unaffected. All additive — no existing item's name or signature changed.

### Server lifecycle

- `Runtime::create_xwayland(&Display, lazy: bool) -> Result<()>` — start the
  lazy (or eager) XWayland server. Errors if called twice.
- `Runtime::xwayland_display_name(&self) -> Option<String>` — the reserved
  `:N` display, available as soon as `create_xwayland` returns (the lazy
  manager reserves the socket up front), so `DISPLAY` can be exported before
  the first client connects.
- `Runtime::set_xwayland_seat(&self)` — point XWayland at the runtime's seat,
  wiring the clipboard/primary/DND bridge.

### Surfaces

- `XwaylandSurface<'h>` with read accessors: `id`, `geometry`, `title`,
  `class`, `instance`, `role`, `pid`, `is_modal`, `override_redirect`,
  `override_redirect_wants_focus`, `has_surface`, and `window_type` →
  `XwaylandWindowType` (normal/dialog/splash/utility/…).
- `XwaylandSurfaceId`, plus `dangling_for_test`/`dangling_nth_for_test`
  constructors for consumers' unit tests.
- State setters on `Runtime`, keyed by id:
  `configure_xwayland_surface(Box2D)`, `set_xwayland_surface_position`,
  `set_xwayland_surface_visible`, `activate_xwayland_surface`,
  `set_xwayland_surface_maximized`, `set_xwayland_surface_fullscreen`,
  `set_xwayland_surface_minimized` (the last writes `_NET_WM_STATE_HIDDEN`),
  and `close_xwayland_surface`.

### Placement, stacking and focus

- `Runtime::raise_xwayland_surface`, `restack_xwayland_surface`,
  `reparent_xwayland_surface_to_band(Band)` — move a surface's scene node
  within/between bands (managed toplevels vs the override-redirect band above
  them).
- `focus_xwayland_surface_keyboard`, `xwayland_surface_has_keyboard_focus`,
  `xwayland_surface_parent` (the transient-parent chain, for dialog
  centering).
- Scene observers for tests: `xwayland_surface_scene_parent_band`,
  `xwayland_surface_scene_position`.
- `Runtime::add_buffer_in_band`/`raise_buffer`/`raise_rect` — band-scoped
  scene primitives that let a consumer build server-side decorations as
  siblings of an X11 window's content.

### Handler callbacks (`ToplevelHandler`, all defaulted)

Additive, defaulted methods so existing implementors compile unchanged:
`xwayland_surface_associate`, `xwayland_surface_mapped`, and the
request/configure/activate/minimize seams the xwm drives.

### Map-race fix

Two races could leave a managed X11 window stuck at the map timeout — a
buffer committed before `associate` (wlroots never maps from that path), and
frame-callback starvation on an undamaged headless output. The wrapper maps a
buffered-but-unmapped surface at `associate`, and adds a per-surface
pre-map commit listener that calls the new
`Runtime::schedule_frame_all(&self) -> usize` so XWayland's handshake frame
callback is answered. Bounded to the handshake commits; never busy-loops.

## 0.20.24 — A2 batch-1 passive compat protocols

Six small, passive globals a compositor turns on once at startup and never
touches again — no new scene or input surface. All additive.

- `Runtime::create_viewporter(&Display) -> Result<()>` — the `wp_viewporter`
  global, letting clients scale and crop a surface's buffer independently of
  its logical size.
- `Runtime::create_fractional_scale_manager(&Display) -> Result<()>` — the
  `wp_fractional_scale_manager_v1` global, so clients can render at a
  non-integer output scale instead of rounding up and downscaling.
  `Runtime::notify_fractional_scale(...)` is the fallback path: a compositor
  that has not (yet) wired per-surface scale tracking can still push a scale
  to a client directly.
- `Runtime::create_single_pixel_buffer_manager(&Display) -> Result<()>` — the
  `wp_single_pixel_buffer_manager_v1` global, letting a client hand over a
  solid-color 1×1 buffer without allocating shared memory for it.
- `Runtime::create_content_type_manager(&Display) -> Result<()>` — the
  `wp_content_type_manager_v1` global, so a client can hint the kind of
  content a surface carries (video, game, …) for presentation tuning.
- `Runtime::create_presentation(&Display) -> Result<()>` — the
  `wp_presentation` global; in wlroots 0.20, creating this global *is* the
  whole presentation-time integration. `Runtime::set_scene_presentation`
  wraps no `wlr_scene_*` symbol (none exists yet in 0.20) — it only enforces
  create-before-init ordering against the presentation global, and is a
  stable extension point for when wlroots grows a scene-level presentation
  API.
- `Runtime::create_xdg_output_manager(&Display) -> Result<()>` — the
  `zxdg_output_manager_v1` global, exposing each output's logical position
  and size (as distinct from its physical mode) to clients that need it for
  layout, such as a panel or a screen-locker.

Errors if any `create_*` is called twice. No existing item's name or
signature changed.

## 0.20.20

Session locking (`ext-session-lock-v1`) and idle management
(`ext-idle-notify-v1` + `zwp_idle_inhibit`). All additive.

- `Band::Lock` — a new topmost scene band above `Overlay`, appended to the
  `Band` enum. Session-lock surfaces render here so they cover all normal
  content and layer-shell while the session is locked.
- `Runtime::create_session_lock_manager(&Display) -> Result<()>` — the
  `ext_session_lock_manager_v1` global, plus a crate-driven state machine that
  enforces the lock's security in one place: while locked, keyboard and pointer
  focus are refused to normal toplevels/layers and routed only to lock
  surfaces; a second lock requested while one is already live is rejected
  (`finished`); and a locker that *dies without unlocking* leaves the session
  locked (an opaque black fill covers every output) rather than exposing the
  desktop. `Runtime::is_session_locked(&self) -> bool` observes the state.
- `SeatHandler::session_lock_changed(&mut self, locked: bool)` — a new
  defaulted method on the existing `SeatHandler` trait telling the compositor to
  suspend its own focus/layout work while locked. Transparently additive:
  existing `SeatHandler` impls inherit the default, so no code change is
  required.
- `Runtime::create_idle_notifier(&Display) -> Result<()>` — the
  `ext_idle_notifier_v1` global. The seat input path reports activity centrally,
  so wlroots drives clients' idle timers with no per-event wiring.
- `Runtime::create_idle_inhibit_manager(&Display) -> Result<()>` — the
  `zwp_idle_inhibit_manager_v1` global; a live inhibitor sets the notifier
  inhibited so idle pauses (e.g. while a video plays).

Errors if any `create_*` is called twice. No existing item's name or signature
changed.

## 0.20.19

Output capture: the `zwlr_screencopy_manager_v1` global.

- `Runtime::create_screencopy_manager(&Display) -> Result<()>` — creates the
  `zwlr_screencopy_manager_v1` global, letting clients capture an output's
  rendered contents (screenshot tools like grim and wf-recorder, and the
  `xdg-desktop-portal-wlr` screen-share path). wlroots owns the whole capture
  flow — buffer negotiation, the copy, damage, and the `ready`/`failed` result
  — so there is nothing further to wire; this mirrors the other
  `create_*_manager` helpers. Errors if called twice (a second global would be
  advertised). Purely additive; no existing item changed.

## 0.20.18

Drag icons now follow the cursor — a behavior fix — plus an accessor to
observe the drag-icon scene node.

- **Fix (behavior):** a visible drag icon now tracks the pointer/touch as it
  moves. `wlr_scene_drag_icon_create` (contrary to a note this crate carried)
  does **not** self-track the cursor — verified against wlroots 0.20.2's own
  `types/scene/drag_icon.c`, its only reposition listener fires on the icon
  surface's buffer-commit deltas, never on motion. So a drag icon was created
  once at `(0, 0)` and never moved, for **every** consumer, not just in tests.
  `on_pointer_motion`/`on_pointer_motion_absolute` and `inject_touch_motion`
  now reposition the icon's scene node to the cursor's layout position on each
  motion (as tinywl and cage do). This is a knowing observable-behavior change
  within `0.20.x` — the second such exception on record (see the compatibility
  note above and `0.20.11`) — because the prior behavior was simply broken.
  A consumer with no drag in progress is unaffected.
- `Runtime::drag_icon_position() -> Option<(i32, i32)>` — the drag icon's
  current layout position while a drag with a visible icon is active, else
  `None`. Public observability (e.g. for tests asserting the icon renders and
  follows). Backed by a stored scene-tree handle that a destroy listener on the
  drag icon's own `events.destroy` clears, so the read can never touch freed
  memory on either drag-teardown path.
- Additive API surface (the new accessor); the repositioning is the behavior
  fix noted above. No existing item's name or signature changed.

## 0.20.17

Completes headless touch drag-and-drop — the touch-side counterpart to what
`0.20.16` shipped for the pointer. Both additions are test-only
(`#[doc(hidden)]`, no production caller); default behavior is unchanged.

- `Runtime::enable_test_touch` — makes the seat advertise
  `WL_SEAT_CAPABILITY_TOUCH` so a headless client can bind `wl_touch`. The
  crate wires no physical touch device (`on_new_input` has no touch arm), so
  a seat driven only by injection never advertised touch, and wlroots refuses
  to create a touch point for a client that holds no `wl_touch` — no injected
  touch drag could start. The capability is folded into the seat's normal
  capability recompute behind a per-`Runtime` flag, so it survives later
  pointer/keyboard hot-plug. Off by default: with `enable_test_touch`
  uncalled, advertised capabilities are byte-identical to `0.20.16`.
- **`inject_touch_motion` now updates touch-point focus.** It calls
  `wlr_seat_touch_point_focus` before `wlr_seat_touch_notify_motion` so the
  touch point's focus surface follows the point as it moves. A touch point's
  focus (unlike a pointer's) is not recomputed per motion, so without this a
  drag kept delivering to the touch-down surface and the destination never
  received `wl_data_device.enter`.
- Additive within `0.20`; no existing item changed.

## 0.20.16

Drag-and-drop, and the virtual pointer + touch injection that let a headless
test drive one.

- `Runtime::create_virtual_pointer_manager` — the
  `zwp_virtual_pointer_manager_v1` global. Its `new_virtual_pointer` event is
  wired to attach the injected pointer to the seat and cursor exactly as a
  physical one from the backend would be (record it, update capabilities,
  register the motion/button listeners), reusing the input-destroy teardown.
  The pointer analogue of `0.20.15`'s virtual keyboard: it mints the pointer
  button serial a serial-gated `wl_data_device.start_drag` needs.
- **Drag-and-drop wiring.** The seat's `request_start_drag` is now honored:
  the grab serial is validated (`wlr_seat_validate_pointer_grab_serial`, then
  touch) and, on success, the matching `wlr_seat_start_pointer_drag` /
  `wlr_seat_start_touch_drag` begins the drag wlroots then drives end to end
  (offer → enter → motion → drop, and the source→destination transfer). An
  unvalidated serial starts no drag. On `start_drag`, a drag icon is rendered
  as a `wlr_scene_drag_icon` node parented in the overlay band so it draws
  above every window.
- `Runtime::inject_touch_down`/`_motion`/`_up` — **test-only** (`#[doc(hidden)]`,
  no production caller): synthesize a touch point through
  `wlr_seat_touch_notify_*` so a headless test can drive a touch drag, since
  touch has no virtual-input protocol of its own.
- Additive within `0.20`; no existing item changed.

## 0.20.15

Virtual keyboard input, and a latent input-device use-after-free closed.

- `Runtime::create_virtual_keyboard_manager` — the
  `zwp_virtual_keyboard_manager_v1` global. Its `new_virtual_keyboard` event is
  wired to attach the injected keyboard to the seat exactly as a physical one
  from the backend would be, so the seat gains keyboard capability and its
  enter/key events mint input serials. This is what lets on-screen keyboards,
  remote-input bridges, and (notably) a headless test harness drive
  serial-gated requests such as `wl_data_device.set_selection`.
- **Fix (soundness):** `InputDevice`'s `alive` backstop flag (an
  `Rc<Cell<bool>>` that each device `Registration` reads from its own `Drop`)
  was declared before those registrations, so field-drop order freed the cell
  first and the reads were a use-after-free. Harmless by luck on a hot-unplug
  (the freed read happened to be `true`, so the unlink still ran), but a bulk
  drop of the input table at shutdown with a device still attached could read
  `false`, skip the unlink, and trip wlroots' `wlr_input_device_finish`
  list-empty assert — aborting the process. `alive` now drops last.
- Additive within `0.20`; no existing item changed.

## 0.20.14

Selection stack: clipboard + primary-selection + data-control globals.

- `Runtime::create_primary_selection_manager` and
  `Runtime::create_data_control_manager` — the
  `zwp_primary_selection_device_manager_v1` and `zwlr_data_control_manager_v1`
  globals. `wl_data_device_manager` was already created in `init_graphics`.
- The seat's `request_set_selection` / `request_set_primary_selection` events
  are now wired to `wlr_seat_set_selection` / `wlr_seat_set_primary_selection`
  (previously dropped), so a keyboard-focused client can own the clipboard and
  the primary selection. Accepted as wlroots delivers them (a valid client
  grant serial), the standard tinywl/sway behavior. data-control is auto-wired
  to the seat's selection by wlroots; no listener of ours is required.
- Additive; no existing item changed.

## 0.20.13

A latent use-after-free closed and the debug-only `Display` pin widened;
no API change, semver-compatible within `0.20`.

- **Fix (memory safety):** `set_layer_surface_output` (`0.20.12`) plants a
  raw `wlr_output*` into the layer surface's role object. Nothing nulled it
  when that output was destroyed, so `LayerSurface::output_id` could read
  freed memory after an output hotplug removal. `forget_output` — the single
  destroy path, always run from the output `destroy` listener — now nulls the
  planted pointer in every tracked layer surface that referenced the dying
  output; `output_id`'s existing null guard then returns `None`. Reachable
  only by assigning an output, destroying it, then calling `output_id`, but a
  genuine dangling deref where it did occur.
- The debug-only `Display`-pin detector (`0.20.12`) now also fires at
  `Backend::run_all` entry — the listener-linking path `Runtime`'s "Lifetime
  obligation" doc names as an unrecoverable use-after-free, which the initial
  `add_rect`/`add_rect_in_band`/`commit_output` sites did not cover. Still
  compiled out under `--release`; no release behavior change.
- Doc corrections: `create_xdg_shell` and `RuntimeInner::pinned_display` now
  describe the shipped detector accurately (its four covered sites, that a
  trip inside a handler aborts rather than unwinds, and the freed-then-
  reallocated-`Display` address-ABA blind spot). No behavioral change.

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
