# `wlr` — safe wrapper over `wlr-sys`

**Date:** 2026-08-03
**Revised:** 2026-08-11 — version selection moved from cargo features to
per-minor branches, after the feature approach was found unresolvable by cargo.
See "Why not cargo features".
**Status:** Approved, implementation in progress
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

`wlr` supports several wlroots minors from one API, and **version selection is a
branch, not a feature**. Each long-lived branch carries a `wlr` whose minor
tracks the wlroots minor it binds, mirroring `wlr-sys` exactly:

| Branch | `wlr` | `wlr-sys` | wlroots | Distro |
|---|---|---|---|---|
| `develop` / `main` | 0.20.x | 0.20 | 0.20 | Arch |
| `support/wlroots-0.19` | 0.19.x | 0.19 | 0.19 | Ubuntu 26.04 |
| `support/wlroots-0.17` | 0.17.x | 0.17 | 0.17 | Ubuntu 24.04 |
| `support/wlroots-0.15` | 0.15.x | 0.15 | 0.15 | Ubuntu 22.04 |

A branch's manifest names exactly one `wlr-sys`:

```toml
[dependencies]
wlr-sys = { version = "0.20", path = "../wlr-sys" }
```

Consumers select by version — `wlr = "0.19"` — and change a version string
rather than their code, because the API is held source-stable across branches.

### Why not cargo features

The obvious design is one published `wlr` with four mutually-exclusive version
features, each gating an optional `wlr-sys`. **Cargo cannot resolve that
manifest**, verified 2026-08-11 in a throwaway workspace: two optional
`wlr-sys` deps at `^0.20` and `^0.19`, with only the 0.20 feature active, fail
before anything compiles.

```
error: failed to select a version for `wlr-sys`.
package `wlr-sys` links to the native library `wlroots`, but it conflicts
with a previous package which links to `wlroots` as well
```

The `links` uniqueness check runs at **resolution**, across every dependency
edge cargo must version-resolve for the lockfile — not across the edges the
enabled features actually activate. Optional deps that are never enabled still
collide.

This is the same property `wlr-sys`'s unsuffixed `links` exists to provide, read
one step further than the original design assumed: cargo rejects *listing* two
minors, not merely *enabling* them. The guard intended to make the
misconfiguration impossible makes the configuration impossible. Suffixing
`links` per minor would restore the feature approach and is not an option — it
would readmit precisely the ABI collision the unsuffixed value prevents.

### The compat boundary

"Stable API where possible, versioned where not" survives the change, but its
mechanism moves: differences between wlroots minors are reconciled by keeping
each branch's public API identical, not by `#[cfg]` inside one crate. There is
no `compat` module — a branch simply calls the wlroots function its own version
has. The recurring cost is unchanged and still has to be relitigated on every
wlroots release; what changes is that a divergence shows up as an API difference
between branches, which the shared test suite catches, rather than as a `cfg`
arm. Known differences already surveyed during the `wlr-sys` backfill:

| Difference | 0.15 | 0.17 | 0.19 | 0.20 |
|---|---|---|---|---|
| Constructors take | `wl_display` | `wl_display` | `wl_event_loop` | `wl_event_loop` |
| `wlr_compositor_create` version param | absent | present | present | present |
| Session subsystem optional | no | yes | yes | yes |
| Vulkan renderer | absent | present | present | present |
| Colour management | absent | absent | present | present |
| pkg-config module | `wlroots` | `wlroots` | `wlroots-0.19` | `wlroots-0.20` |
| Output commit | `wlr_output_commit` | both | `wlr_output_commit_state` | `wlr_output_commit_state` |

The commit row is the **first confirmed divergence**, found while writing the
implementation plan: wlroots replaced the implicit pending-state model with an
explicit `wlr_output_state`, so 0.15 has only `wlr_output_commit`, 0.19+ have only
`wlr_output_commit_state`, and 0.17 carries both during the transition. Under
branch selection this is not a `cfg` arm: `develop` and the 0.19/0.17 branches
call `wlr_output_commit_state`, the 0.15 branch calls `wlr_output_commit`, and
`Output::commit` presents the same signature on all four.

The discipline the `compat` module was meant to enforce still applies —
divergence must stay inside method bodies. The moment a difference reaches a
public signature, consumers can no longer move between branches by changing a
version string, which is the whole point. That is the signal the boundary was
drawn in the wrong place, not a reason to let the signature drift.

## 2. Handles are borrow-scoped and unforgeable

```rust
pub struct Output<'h> {
    raw: NonNull<sys::wlr_output>,
    id: Option<OutputId>,
    _scope: PhantomData<&'h ()>,
}
```

The lifetime is bound to the dispatch call. The constructor is `pub(crate)`, so a
consumer cannot manufacture one — a `&Output` that escapes a handler is a
**compile error**, not a documented rule.

Not `#[repr(transparent)]`: `id` caches the `OutputId` dispatch already resolved
to look the output up in the session registry, so `Output::id` is a field read
rather than an `wlr_addon_find` walk repeated on every call — the ergonomic this
crate is built around, since consumers key their own state by id on essentially
every event. `id` is private, so adding it does not affect the frozen public
API.

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
   commit).

### The cost of consequence 2, found during implementation

Point 2 originally said such paths "dispatch directly". **They cannot**, and this
is the first real cost the deferral decision has imposed.

Delivering `frame` directly while another handler is running would hand out a
second `&mut State` while the first is still live — precisely the aliasing UB
deferral exists to prevent. Soundness is not negotiable against a
timeliness contract, so `frame` is deferred like every other event, and a frame
arriving while another handler runs is delivered *after* that handler returns,
outside the window wlroots intended it for.

So the category of never-deferred paths does not exist in this design. The
rendering slice inherits this: making frames timely requires changing the
dispatch model — a second borrow discipline for render-critical paths, say — not
special-casing `frame` against the current one. `OutputHandler::frame`'s
documentation states the limitation rather than promising what the crate cannot
deliver.

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
| Headless integration | Real backend, real announcement, real teardown — mirroring `wlr-sys`'s `examples/headless.rs`. Not a real *frame*: see the revision under "Scope". |
| Version matrix in CI | Each branch's own distro container builds and tests that branch's `wlr`, so a divergence that reaches the public API fails on the affected branch. |

## 7. Scope

**In this spec:** `Display`, `EventLoop`, `Backend`, `Output`, the dispatch core,
deferral, IDs, version selection, `Error`. Ends at a headless backend announcing
an output to a handler, and tearing down cleanly.

**Revised during implementation:** this originally read "Ends at a headless
backend rendering a frame." It does not, and cannot. Making an output produce a
frame means enabling it and setting a mode, which means exposing the
`wlr_output_state` setters — and those belong to the rendering slice, not this
one. `Output::commit` therefore commits an *empty* state, which is a no-op on a
disabled output, and the headless integration test asserts zero frames rather
than one. Frame *delivery* is covered against a real `wl_signal` in `backend.rs`'s
own tests; frame *production* waits.

Two things settled during implementation that the design did not anticipate:

- **`Backend` has no public `start`.** `wlr_backend_start` announces existing
  outputs *synchronously* — `backend.h` documents that starting "may signal
  new_input or new_output immediately" — so a compositor that starts the backend
  before linking its listeners silently never hears about them. `run` therefore
  owns starting, after wiring. A public `start` whose only failure mode is
  silence was not worth keeping.
- **The registry mapping `OutputId` back to a live output is per-`run`**, not
  process-wide. Per-output listeners name a `Dispatcher<S>` and a `&mut State`
  that exist only for that call, and `EventLoop::dispatch` is public safe API, so
  a longer-lived registry would be a use-after-free reachable without `unsafe`.
  The cost is that outputs are not re-announced by a later `run`, since wlroots
  offers no enumeration API.

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
| One published `wlr` with mutually-exclusive version features | **Impossible.** Cargo's `links` check rejects the manifest at resolution, however few features are enabled. This was the original choice; see "Why not cargo features". |
| Separate crate names per minor (`wlr-020`, `wlr-019`, …) | Resolves, but costs four crates.io names and four publishes per release, and consumers change an import path rather than a version string — worse ergonomics than branches for no extra capability. |
