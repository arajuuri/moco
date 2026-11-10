#!/bin/bash
# Check that a macOS moco binary loads on 10.7 (Lion): minimum OS 10.7, an LC_UNIXTHREAD entry point, only libSystem
# and libiconv linked, and no symbols newer than 10.7. See docs/14-macos-10.7.md. Used by CI and the release workflow.
#
#   scripts/check-macos-binary.sh target/release/moco
set -eu
bin="${1:?usage: $0 BINARY}"
otool -l "$bin" | grep -A3 -E 'LC_BUILD_VERSION|LC_VERSION_MIN_MACOSX' || true
minos=$(otool -l "$bin" | awk '/LC_VERSION_MIN_MACOSX/{f=1} f&&$1=="version"{print $2; exit}')
[ -n "$minos" ] || { echo "no LC_VERSION_MIN_MACOSX: Lion's loader cannot start a binary without it"; exit 1; }
# awk, not `sort -V`, which Lion's sort lacks.
awk -v v="$minos" 'BEGIN { split(v, a, "."); exit !(a[1] < 10 || (a[1] == 10 && (a[2] < 7 || (a[2] == 7 && a[3] + 0 == 0)))) }' || { echo "minos $minos > 10.7"; exit 1; }
echo "minos=$minos"
# LC_MAIN exists only from 10.8: Lion's kernel finds no entry point and the binary crashes at launch (#113).
if otool -l "$bin" | grep -q 'cmd LC_MAIN$'; then echo "entry point is LC_MAIN, which Lion cannot start; needs LC_UNIXTHREAD"; exit 1; fi
otool -l "$bin" | grep -q 'cmd LC_UNIXTHREAD$' || { echo "no LC_UNIXTHREAD entry point"; exit 1; }
echo "entry=LC_UNIXTHREAD"
libs=$(otool -L "$bin" | tail -n +2 | awk '{print $1}' | grep -vE '^/usr/lib/(libSystem\.B|libiconv\.2)\.dylib$' || true)
[ -z "$libs" ] || { echo "links more than libSystem and libiconv:"; echo "$libs"; exit 1; }
newer=$(nm -u "$bin" | grep -E 'getentropy|clock_gettime|os_unfair_lock|ulock' || true)
[ -z "$newer" ] || { echo "uses symbols newer than 10.7:"; echo "$newer"; exit 1; }
