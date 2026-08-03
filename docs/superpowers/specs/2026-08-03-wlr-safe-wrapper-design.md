# `wlr` — safe wrapper over `wlr-sys`

**Date:** 2026-08-03
**Status:** Approved, not yet implemented
**Scope:** First of several specs. This one covers the core plus output bring-up.

## Purpose

`wlr` is a general-purpose ecosystem crate: other people build compositors on it.
That rules out opinionated shortcuts — the API has to be usable by compositors
whose architecture we cannot predict.

The predecessor matters here. The original `wlroots-rs` was sound but was
abandoned mid-0.x because its `handle.with(|obj| ...)` access pattern was too
painful to write compositors in. Ergonomics is a correctness requirement for this
crate, not a nicety.

## The problem this design exists to solve

The 0.20 bindings expose **117 `wlr_*_create` functions but only 38
`wlr_*_destroy`**, and **106 types carry a `destroy` signal**. Almost everything
you touch is therefore owned by C: handed to you in a callback, and freed
whenever wlroots decides — announced through that signal.

A `&wlr_output` that outlives its destroy signal is a use-after-free, and no
amount of care at the call site prevents it. Every decision below follows from
that.

## 1. Crate layout and version selection

`crates/wlr/` joins the existing workspace. The name `wlr` was verified available
on crates.io on 2026-08-03.

`wlr` supports several wlroots minors from one API. Because `wlr-sys` declares
`links = "wlroots"`, **cargo rejects any dependency graph containing two
`wlr-sys` minors**, so version selection must be a build-time choice inside
`wlr` rather than parallel dependencies:

```toml
[features]
default = ["wlroots-0-20"]
wlroots-0-20 = ["dep:wlr-sys-020"]
wlroots-0-19 = ["dep:wlr-sys-019"]
wlroots-0-17 = ["dep:wlr-sys-017"]
wlroots-0-15 = ["dep:wlr-sys-015"]

[dependencies]
wlr-sys-020 = { package = "wlr-sys", version = "0.20", optional = true }
wlr-sys-019 = { package = "wlr-sys", version = "0.19", optional = true }
wlr-sys-017 = { package = "wlr-sys", version = "0.17", optional = true }
wlr-sys-015 = { package = "wlr-sys", version = "0.15", optional = true }
```

Enabling two features is rejected by cargo automatically, via the `links`
collision. That is the misconfiguration being *impossible* rather than merely
discouraged. A `compile_error!` covers the zero-feature case with a readable
message, since cargo's own error there is unhelpful.

Everything in the crate imports `crate::sys`, a module that re-exports whichever
`wlr-sys` was selected. Version differences live in `crate::compat` **and nowhere
else**. That module is the entire "versioned where not" surface; confining it is
what keeps the ongoing cost visible instead of smeared across the crate.

### The compat boundary

"Stable API where possible, versioned where not" is the option with a recurring
cost: the boundary has to be relitigated on every wlroots release. Known
differences already surveyed during the `wlr-sys` backfill:

| Difference | 0.15 | 0.17 | 0.19 | 0.20 |
|---|---|---|---|---|
| Constructors take | `wl_display` | `wl_display` | `wl_event_loop` | `wl_event_loop` |
| `wlr_compositor_create` version param | absent | present | present | present |
| Session subsystem optional | no | yes | yes | yes |
| Vulkan renderer | absent | present | present | present |
| Colour management | absent | absent | present | present |
| pkg-config module | `wlroots` | `wlroots` | `wlroots-0.19` | `wlroots-0.20` |
| Output commit | `wlr_output_commit` | both | `wlr_output_commit_state` | `wlr_output_commit_state` |

The commit row is the **first confirmed `compat` entry**, found while writing the
implementation plan: wlroots replaced the implicit pending-state model with an
explicit `wlr_output_state`, so 0.15 has only `wlr_output_commit`, 0.19+ have only
`wlr_output_commit_state`, and 0.17 carries both during the transition. `compat`
builds 0.17 with the newer path, so three of four versions share one code path.

If `compat` starts sprawling beyond adapters of this kind, that is the signal the
boundary was drawn in the wrong place — not a reason to add more adapters.

## 2. Handles are borrow-scoped and unforgeable

```rust
#[repr(transparent)]
pub struct Output<'h> {
    raw: NonNull<sys::wlr_output>,
    _scope: PhantomData<&'h ()>,
}
```

The lifetime is bound to the dispatch call. The constructor is `pub(crate)`, so a
consumer cannot manufacture one — a `&Output` that escapes a handler is a
**compile error**, not a documented rule.

Long-lived state belongs to the consumer, keyed by ID:

```rust
struct App { outputs: HashMap<OutputId, MyOutput> }

impl OutputHandler for App {
    fn frame(&mut self, out: &Output<'_>) {
        self.outputs.get_mut(&out.id()).unwrap().frames += 1;
        out.commit();
    }
    fn destroyed(&mut self, id: OutputId) {
        self.outputs.remove(&id);
    }
}
```

The cost, stated honestly: consumers key their own state, and operations spanning
two objects require looking both up. That is the price of dangling being
impossible.

### Identity

`OutputId` is stable, comparable and storable. It is **not** the pointer value:
wlroots can reuse an address after free, so pointer identity is unsound across a
destroy.

IDs come from a monotonic counter attached with `wlr_addon` — wlroots' own
mechanism for data whose lifetime is bound to an object. It self-cleans on
destroy, so there is no side table to go stale.

**Verified 2026-08-03:** `wlr_addon_init`, `wlr_addon_finish`, `wlr_addon_find`,
`wlr_addon_set_init` and `wlr_addon_set_finish` exist with identical signatures
in all four supported wlroots versions (0.15, 0.17, 0.19, 0.20). The ID mechanism
needs no compat fallback.

## 3. Dispatch core

One `&mut State` reaches handlers. The dispatcher stores `*mut S` for the
duration of a call and recovers it in the C callback.

Listeners are `#[repr(C)]` structs embedding `wl_listener`, recovered with
`wlr_sys`'s `container_of!`. Note the field-path distinction that already caused
a bug in `wlr-sys`: `container_of!` is handed a `*mut wl_listener` and takes the
listener field; `wl_list_for_each!` walks the `link` *inside* it. Those offsets
coincide only because `wl_listener.link` is declared first.

### Reentrancy

wlroots emits signals synchronously from inside API calls — certainly on every
destroy path, which `wlr-sys`'s `tests/signal.rs` demonstrates directly. A
handler that destroys an object therefore re-enters dispatch while `&mut State`
is already live. That aliases `&mut` and is undefined behaviour.

`wlr` detects and defers:

```rust
if self.in_dispatch.get() {
    self.deferred.borrow_mut().push_back(event);
    return;
}
```

The queue drains after the outer handler returns.

Two consequences that fall out of this and must be designed for, not patched
later:

1. **A deferred event may name an object destroyed before delivery.** Deferred
   events therefore carry an `OutputId`, never a handle, and re-resolve at
   delivery — dropping silently if the object is gone.
2. **Anything wlroots requires the compositor to do before a callback returns
   cannot be deferred.** That set is small and enumerable (frame timing, buffer
   commit); those paths dispatch directly and are documented as such.

## 4. Handler traits

```rust
pub trait OutputHandler {
    fn new_output(&mut self, output: &Output<'_>) {}
    fn frame(&mut self, output: &Output<'_>) {}
    fn destroyed(&mut self, id: OutputId) {}
}
```

All methods defaulted, so consumers implement only what they use.
`EventLoop::run(&mut state)` requires the trait bounds for the subsystems in
scope. This matches Smithay's shape, so the ecosystem idiom is familiar.

## 5. Errors

wlroots signals failure by returning null or `false`, with no detail available.
Constructors return `Result<_, Error>` where `Error` names the operation that
failed. There is nothing more truthful to report; inventing detail would be
worse than admitting the API does not provide it.

Library paths never panic except on a violated safety contract.

## 6. Testing

The safety claim is tested as an invariant, not assumed:

| Test | Proves |
|---|---|
| `trybuild` compile-fail | `&Output` cannot escape a handler. The entire design rests on this. |
| Reentrancy test | A handler destroying an output from inside `frame` defers correctly, and a deferred event for a destroyed object is dropped rather than delivered. |
| Headless integration | Real backend, real frame, real teardown — mirroring `wlr-sys`'s `examples/headless.rs`. |
| Version matrix in CI | The existing distro containers build `wlr` against each wlroots minor, so a version-selection or compat mistake fails on the affected distro. |

## 7. Scope

**In this spec:** `Display`, `EventLoop`, `Backend`, `Output`, the dispatch core,
deferral, IDs, version selection, `Error`. Ends at a headless backend rendering a
frame.

**Later specs, in likely order:** seat and input; xdg-shell; layer-shell; scene
graph; renderer surface beyond output bring-up.

Splitting this way means a mistake in the lifetime model is found while the API
is one subsystem wide, rather than after four subsystems are built on it.

## Rejected alternatives

| Option | Why not |
|---|---|
| `handle.with(\|obj\| ...)` | Sound and storable, but nested closures at every access, and two objects require two nestings. This is the pattern that got the predecessor abandoned. |
| Weak handles checked on access | Every access is a fallible `Option` forever, plus registry bookkeeping on every create and destroy. |
| Addon-owned Rust state | Native to wlroots, but the addon owns the data, forcing interior mutability and careful reentrancy throughout consumer code. |
| Per-object closures | Pushes `Rc<RefCell<..>>` into every consumer for shared state — surrenders the borrow-scoped model's main benefit immediately. |
| Enum events from a queue | wlroots callbacks are synchronous and some must act before returning; queueing cannot express those faithfully. |
| Panic on reentrancy | Sound, but turns a legitimate wlroots pattern into a runtime crash consumers must design around. |
| One `wlr` minor per wlroots minor | Honest and cheap, but leaves consumers rewriting code per distro — the thing an ecosystem crate exists to prevent. |
