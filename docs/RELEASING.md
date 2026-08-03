# Releasing / bumping the wlroots minor

wlroots breaks API on **every** minor release, and this crate's versioning policy
is `wlr-sys 0.N.x` binds wlroots `0.N.x`. So bumping the wlroots minor is the
most frequent maintenance task this repo has, and it touches more than one file.

Two of these sites are self-enforcing — `tests/link.rs` fails if the crate
version and `WLR_VERSION_MINOR` disagree, and `cargo` fails if `links` collides.
The rest are not. Work the list.

## The trap

`build.rs` derives everything from `WLROOTS_MINOR` / `WLROOTS_NEXT_MINOR`
precisely so that a naive `sed 's/0.20/0.21/'` cannot produce
`range_version("0.21".."0.21")` — an empty half-open range that rejects *every*
installed wlroots while reporting "wrong minor installed", i.e. telling the user
to install the version they already have. Keep both constants; do not inline
either.

## Checklist

| # | File | What changes |
|---|---|---|
| 1 | `crates/wlr-sys/build.rs` | `WLROOTS_MINOR`, `WLROOTS_NEXT_MINOR`, and the `concat!` literal in `WLROOTS_PC` |
| 2 | `crates/wlr-sys/Cargo.toml` | `version` (minor must match), `description` |
| 3 | `crates/wlr-sys/protocol/` | Re-vendor the XML from the `wlr-protocols` revision the new wlroots pins; update `PROVENANCE.md` and `SHA256SUMS` |
| 4 | `crates/wlr-sys/prebuilt/bindings-docsrs.rs` | Regenerate (see below) |
| 5 | `.github/workflows/ci.yml` | The `wlroots0.NN` pacman package name |
| 6 | `crates/wlr-sys/README.md` | Version references in the intro, Requirements and Versioning sections |
| 7 | `README.md` (root) | Version reference in the intro |
| 8 | `docs/superpowers/specs/…-design.md` | Nothing — it is a dated historical record, not living documentation |

`links = "wlroots"` is deliberately **not** version-suffixed and must stay that
way: cargo enforces one package per `links` value, and that is precisely what
stops a graph from containing two `wlr-sys` minors — which would otherwise link
`libwlroots.so` and `libwlroots-0.21.so` together, where identical symbol
names bind to whichever wins interposition and struct offsets silently disagree.

Verify with:

```sh
rg -n '0\.(2[0-9])' --glob '!target' --glob '!*.lock'
```

## Regenerating the docs.rs bindings snapshot

docs.rs has no wlroots, so `build.rs` falls back to a committed snapshot when
`DOCS_RS` is set. Regenerate it whenever the bound API changes:

```sh
cargo build -p wlr-sys --all-features
newest=$(find target/debug/build -maxdepth 3 -name bindings.rs -path '*wlr-sys*' \
  -printf '%T@ %p\n' | sort -rn | head -1 | cut -d' ' -f2-)
cp "$newest" crates/wlr-sys/prebuilt/bindings-docsrs.rs
DOCS_RS=1 cargo doc -p wlr-sys --no-deps --all-features   # confirm it renders
```

CI regenerates and diffs this file, so a stale snapshot is a build failure rather
than silently wrong documentation.

## Before publishing

```sh
cargo package -p wlr-sys            # builds from a clean extraction
cargo publish -p wlr-sys --dry-run
```

`cargo package` is the step that catches a missing `protocol/*.xml`, an
`include_str!` escaping the package root, or a `build.rs` reading outside
`CARGO_MANIFEST_DIR` — all of which look fine in-tree. A version can only be
yanked, never replaced, so this is not optional.

## Frozen within a minor

Under cargo's 0.x rules, `0.15.0 → 0.15.1` is an automatic upgrade for every
consumer. So within a wlroots minor, the crate's **hand-written** surface is
frozen: `wl_list_iter`, `container_of!`, `wl_list_for_each!`,
`wl_signal_emit_mutable`, the feature names, the `wlr_has_*` cfg names, the
`DEP_WLROOTS_*` metadata keys, and the blocklist (which determines type
*identity* for `wl_listener` and friends). Breaking changes to any of those wait
for the next wlroots minor.
