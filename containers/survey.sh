#!/usr/bin/env bash
# Dumps everything wlr-sys's build.rs needs to know about the wlroots this
# image ships. Run inside each distro container; the output drives the
# per-version SUBSYSTEMS table, blocklist and gated-header list.
set -uo pipefail

echo "### distro"
. /etc/os-release; echo "$PRETTY_NAME"

echo "### pkg-config module name"
PC=""
for cand in $(pkg-config --list-all 2>/dev/null | awk '$1 ~ /^wlroots/ {print $1}'); do
  echo "found: $cand"; PC="${PC:-$cand}"
done
[ -z "$PC" ] && { echo "NO wlroots .pc FOUND"; exit 1; }
echo "using: $PC"

echo "### version"
pkg-config --modversion "$PC"

echo "### soname"
libdir=$(pkg-config --variable=libdir "$PC")
ls "$libdir"/libwlroots*.so* 2>/dev/null | head -5

echo "### have_* variables in the .pc"
pcdir=$(pkg-config --variable=pcfiledir "$PC")
grep -E '^have_' "$pcdir/$PC.pc" 2>/dev/null || echo "(none - this version does not publish have_* flags)"

echo "### all .pc variables"
pkg-config --print-variables "$PC" 2>/dev/null | sort | tr '\n' ' '; echo

echo "### include dir"
incdir=$(pkg-config --variable=includedir "$PC")
root=$(pkg-config --cflags-only-I "$PC" | tr ' ' '\n' | sed 's/^-I//' | grep -m1 wlr || echo "$incdir")
echo "$root"

echo "### header count"
find "$root/wlr" -name '*.h' 2>/dev/null | wc -l

echo "### headers"
find "$root/wlr" -name '*.h' 2>/dev/null | sed "s|$root/||" | sort

echo "### WLR_USE_UNSTABLE required?"
if grep -rq 'WLR_USE_UNSTABLE' "$root/wlr" 2>/dev/null; then echo "YES"; else echo "no"; fi

echo "### config.h flags"
cat "$root/wlr/config.h" 2>/dev/null | grep -E '^#define WLR_HAS' || echo "(no config.h)"

echo "### quoted includes (generated protocol headers wlroots does NOT install)"
grep -rhoE '#include "[^"]+"' "$root/wlr" 2>/dev/null | sed 's/#include "//; s/"//' | sort -u | while read -r h; do
  if [ -f "/usr/include/$h" ] || [ -f "$root/$h" ]; then echo "PRESENT $h"; else echo "MISSING $h"; fi
done

echo "### static inline in public headers (would need a C shim)"
grep -rc 'static inline' "$root/wlr" 2>/dev/null | grep -v ':0$' | head -10 || echo "(none)"

echo "### external includes needing extra -I"
grep -rhoE '#include <(EGL|GLES2|vulkan|xcb|libinput|drm)[^>]*>' "$root/wlr" 2>/dev/null | sort -u
