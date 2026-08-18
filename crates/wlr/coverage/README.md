# The coverage ledgers

Two files, one row per wlroots symbol, no exceptions: `wrapped.toml` for what the
safe API covers, `waived.toml` for what it deliberately does not. Ground truth is
the `bindings.rs` bindgen generated for the current build, so a wlroots patch
release that adds a symbol fails `cargo test -p wlr --test coverage_audit` on its
own. The parser and the gate live in `../src/coverage.rs`; `cargo xtask coverage`
prints the burn-down from the same code.

## Scope

Only `wlr_*`-prefixed symbols are audited. pixman, EGL, Vulkan, GL, xkbcommon and
libinput types reach `bindings.rs` transitively through wlroots' own structs and
are out of scope **by construction**, not by waiver — this crate wraps wlroots'
*use* of them and never re-wraps the foreign library. That is what the
`foreign-library` reason is reserved for should a `wlr_`-prefixed symbol ever
qualify; none does today.

`wlr_log()` and `_wlr_log`/`_wlr_vlog` are absent from `bindings.rs` entirely:
`wlr_log` is a variadic macro and the two underscore-prefixed functions are
`static inline`. They have no rows in either ledger because there is nothing to
reconcile. `wlr_log_init` and `wlr_log_get_verbosity` do exist and do have rows.

A handful of names denote both a type and a function (`wlr_xdg_toplevel_configure`
is both a struct and a function in C, which is legal there). The ledgers are keyed
by name, so one row covers both.

## When does a symbol count as wrapped?

A row in `wrapped.toml` names the safe public item that gives a consumer that
symbol's *effect*. Internal plumbing does not count: `wlr_addon_init` is called on
every toplevel this crate tracks, but no consumer can attach an addon, so it is
waived until M5 exposes one. Nor does raw-pointer reachability — `as_ptr()` is an
interop tool, not coverage.

Every row is validated twice by the gate: the symbol must exist in `bindings.rs`,
and it must still appear in a `sys::` use somewhere under `crates/wlr/src`. The
second check is what stops a row from outliving the wrapper it describes.

## Editing

```toml
# wrapped.toml
[[wrapped]]
symbol = "wlr_scene_node_set_position"
module = "runtime"                     # crates/wlr/src/<module>.rs
item   = "Runtime::set_rect_position"  # what a consumer calls

# waived.toml
[[waived]]
symbol    = "wlr_renderer_init"
reason    = "interface-impl-only"
note      = "Reachable only by implementing a wlroots vtable from Rust."
milestone = "M13"                      # required only for reason = "not-yet"
```

The reader is a deliberately dumb, deliberately strict mini-TOML: `[[wrapped]]` /
`[[waived]]` headers, `key = "value"` lines with no escapes, `#` comments, blank
lines. Anything else is a parse error naming the line. There is no `toml`
dependency on purpose — a general parser would accept syntax these files have no
meaning for.

Reasons: `not-yet` (the backlog; needs a milestone), `interface-impl-only`,
`deprecated`, `internal`, `superseded-by`, `foreign-library`. **100% coverage is
defined as `waived.toml` containing zero `not-yet` rows**; that is the assertion
in the `#[ignore]`d `coverage_is_one_hundred_percent` test, un-ignored at M13.

When a milestone wraps a symbol, move its row from `waived.toml` to
`wrapped.toml` **in the same commit as the wrapper** — a row in both files is a
hard failure, which is what keeps the two steps from drifting apart.

## Feature configurations

`bindings.rs` for a smaller feature set is a strict subset of the all-features
one, so a ledger row for a gated symbol has nothing to match against there.
Accordingly the "ledger names a symbol bindgen never emitted" check is a hard
failure **only** when `WLR_COVERAGE_ALL_FEATURES=1` is set, which CI does after
building `wlr-sys --all-features`. The other three checks gate in every
configuration.

Ground truth is otherwise the newest `bindings.rs` under `target/`, which is a
trap worth knowing: building `wlr-sys --all-features` and *then* running anything
that builds it in another configuration leaves the subset newest. Set
`WLR_COVERAGE_BINDINGS` to an exact path to settle it — CI resolves it once,
immediately after the all-features build, for exactly this reason.
