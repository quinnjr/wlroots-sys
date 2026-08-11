# Distro containers

Ubuntu pins a different wlroots minor per release, and wlroots breaks ABI on
every minor. Since `wlr-sys` binds exactly one minor per crate minor, supporting
Ubuntu means backfilling a `wlr-sys` release per wlroots version.

These images provide a build environment per release, and `survey.sh` dumps
everything `build.rs` needs to know about the wlroots that release ships.

## Usage

```sh
# 22.04 and 24.04 ship an unversioned libwlroots-dev; 26.04 versioned it.
docker build --build-arg UBUNTU_REF=22.04 -f containers/Dockerfile.ubuntu -t wlr-sys-ubuntu:22.04 .
docker build --build-arg UBUNTU_REF=24.04 -f containers/Dockerfile.ubuntu -t wlr-sys-ubuntu:24.04 .
docker build --build-arg UBUNTU_REF=26.04 --build-arg WLROOTS_DEV_PKG=libwlroots-0.19-dev \
  -f containers/Dockerfile.ubuntu -t wlr-sys-ubuntu:26.04 .

docker run --rm wlr-sys-ubuntu:24.04                      # survey
docker run --rm -v "$PWD:/src" -w /src wlr-sys-ubuntu:24.04 cargo test   # build the crate
```

### Building `wlr` against a specific wlroots

`wlr`'s minor tracks the wlroots minor it binds, and each lives on its own
branch, so verifying one means checking out that branch and using its distro's
container. **This is forward-looking**: `wlr` exists only on `develop`/`main`
(wlroots 0.20, Arch) as of this writing and has not yet been cherry-picked to
any `support/*` branch, so the command below cannot actually be run yet — it
documents the procedure for when it is.

```sh
git checkout support/wlroots-0.19
docker run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/tmp/t \
  wlr-sys-ubuntu:26.04 cargo test -p wlr
```

## Survey results (2026-08-03)

| | Ubuntu 22.04 | Ubuntu 24.04 | Ubuntu 26.04 | Arch (reference) |
|---|---|---|---|---|
| codename | jammy | noble | resolute | — |
| wlroots | **0.15.1** | **0.17.1** | **0.19.2** | 0.20.2 |
| pkg-config name | `wlroots` | `wlroots` | `wlroots-0.19` | `wlroots-0.20` |
| dev package | `libwlroots-dev` | `libwlroots-dev` | `libwlroots-0.19-dev` | `wlroots0.20` |
| headers | 93 | 107 | 119 | 123 |
| `have_*` flags | 6 | 8 | 10 | 10 |
| `-DWLR_USE_UNSTABLE` | required | required | required | required |
| `static inline` in headers | none | none | none | none |
| generated protocol headers needed | 6 | 9 | 10 | 2 |

### What this means for the backfill

**The pkg-config module name is unversioned before 0.19.** `wlroots` on 22.04 and
24.04, `wlroots-0.19` on 26.04. `WLROOTS_PC` cannot be derived from the minor
alone, and on those releases two wlroots versions cannot coexist.

**`have_*` flags accumulate.** 0.15 has no `have_session`, `have_gbm_allocator`,
`have_udmabuf_allocator` or `have_color_management`; 0.17 adds session and gbm;
0.19 adds the rest. Each backfill needs its own `SUBSYSTEMS` table — rows cannot
simply be copied down.

**Ubuntu 22.04 reports `have_vulkan_renderer=false`.** That makes it the first
environment to actually exercise the feature-enabled-but-unavailable degradation
path, which no other CI configuration reaches.

**Protocol generation is the big one.** wlroots 0.20 needs 2 generated protocol
headers; 0.19 needs 10, 0.17 needs 9, 0.15 needs 6. Almost all of the extras come
from `wayland-protocols` (xdg-shell, tablet, pointer-constraints, content-type,
cursor-shape, tearing-control, ext-image-copy-capture, color-management) rather
than `wlr-protocols` — and all of those are resolvable by name from
`pkg-config --variable=pkgdatadir wayland-protocols`. Only `wlr-layer-shell-unstable-v1`
and `wlr-output-power-management-unstable-v1` need vendoring.

So `build.rs` should *discover* what to generate — scan the installed headers for
quoted `#include "…-protocol.h"`, resolve each against the vendored directory and
then the wayland-protocols data directory — rather than hardcoding a list. That
adapts across versions and is strictly better than the hardcoded pair even for
0.20.

## Raw survey output

`survey.sh` prints the full header list, `.pc` variables, `config.h` flags, and
external include requirements. Re-run it after any distro update; these numbers
are a snapshot.
