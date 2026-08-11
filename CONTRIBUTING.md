# Contributing

## Branching model

This repository follows [git-flow](https://nvie.com/posts/a-successful-git-branching-model/),
with one wrinkle that matters: **`wlr-sys` binds one wlroots minor per crate
minor**, and several of those minors are supported at once. So the older release
lines are not history — they are live, maintained branches.

| Branch | Role |
|---|---|
| `main` | Release only. Every commit is a published version and carries a `vX.Y.Z` tag. Never commit here directly. |
| `develop` | Integration branch and the default for PRs. Tracks the newest wlroots (currently 0.20). |
| `feature/*` | Branch from `develop`, merge back to `develop`. |
| `release/*` | Branch from `develop` when cutting a version; merge to `main` **and** back to `develop`; tag on `main`. |
| `hotfix/*` | Branch from `main`, merge to `main` **and** `develop`. |
| `support/wlroots-N.M` | Long-lived maintenance for an older wlroots minor. See below. |

### Support branches are the important part here

| Branch | wlroots | Distro that ships it |
|---|---|---|
| `develop` / `main` | 0.20 | Arch |
| `support/wlroots-0.19` | 0.19 | Ubuntu 26.04 LTS |
| `support/wlroots-0.17` | 0.17 | Ubuntu 24.04 LTS |
| `support/wlroots-0.15` | 0.15 | Ubuntu 22.04 LTS |

These are **not** behind `develop` and will never be merged into it. wlroots
breaks its API every minor, so the branches have genuinely different
`SUBSYSTEMS` tables, gated headers, blocklists and tests — see each branch's
`CLAUDE.md` and the commit that created it. A fix that applies to more than one
line gets cherry-picked, not merged.

Releases from a support branch are tagged on that branch, not on `main`; `main`
only ever carries the newest line.

### Cherry-picking across lines

Fixes to genuinely shared code — `build.rs` plumbing, `src/list.rs`,
`src/signal.rs`, container tooling — usually apply everywhere. Fixes that touch a
`SUBSYSTEMS` row, a gated header, or a test signature usually do not, because
those differ per wlroots version.

Always re-verify in the target branch's own container. The differences that bite
are invisible on the host:

```sh
docker run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/tmp/t \
  wlr-sys-ubuntu:24.04 cargo test --workspace
```

## Using the git-flow tool (optional)

The model does not require it, but if you have `git-flow` installed:

```sh
git flow init -d
git config gitflow.prefix.versiontag v   # the one non-default setting
```

The defaults match this repo — production `main`, development `develop`,
prefixes `feature/`, `release/`, `hotfix/`, `support/` — except the version tag
prefix, which `git flow init -d` leaves empty while our tags are `vX.Y.Z`.

Note this config lives in `.git/config` and is not shared, so each clone sets it
independently. Nothing depends on the tool; the branch names are the contract.

## Before opening a PR

Target `develop` (or the relevant `support/*` branch). CI runs the full matrix,
but these are the ones worth running first:

```sh
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
cargo test --workspace
cargo test -p wlr                              # the safe wrapper, this branch's wlroots
cargo +1.88 check --workspace --all-features   # the declared MSRV
```

`CLAUDE.md` documents the architecture and the traps — particularly why the
bindgen blocklist is load-bearing and fails silently, and why subsystem detection
reaches downstream as `DEP_WLROOTS_*` rather than as cfgs.

## Releasing

See [`docs/RELEASING.md`](docs/RELEASING.md). Two things that are easy to get
wrong and cannot be undone:

- A crates.io version can only be yanked, never replaced. Run `cargo package`
  from the branch's container first — it builds from a clean extraction, which is
  where a missing vendored file shows up.
- **Verify the published artifact, not just the repo.** Install it from crates.io
  into a fresh consumer crate in the matching container. The repo carries an
  untracked `Cargo.lock` and a consumer does not, so dependency-resolution
  problems are invisible until you test as a user. That is exactly how the first
  four releases shipped an unreachable MSRV claim.
