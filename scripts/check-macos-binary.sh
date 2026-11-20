#!/bin/bash
# Check that a macOS moco binary loads on 10.7 (Lion): minimum OS 10.7, an LC_UNIXTHREAD entry point, only libSystem
# and libiconv linked, no symbols newer than 10.7, and no SSE4 code for the oldest Core 2 Duos. See docs/14-macos-10.7.md.
# Used by CI and the release workflow.
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
# LC_MAIN exists only from 10.8: Lion's kernel finds no entry point and the binary crashes at launch (archive#113).
if otool -l "$bin" | grep -q 'cmd LC_MAIN$'; then echo "entry point is LC_MAIN, which Lion cannot start; needs LC_UNIXTHREAD"; exit 1; fi
otool -l "$bin" | grep -q 'cmd LC_UNIXTHREAD$' || { echo "no LC_UNIXTHREAD entry point"; exit 1; }
echo "entry=LC_UNIXTHREAD"
libs=$(otool -L "$bin" | tail -n +2 | awk '{print $1}' | grep -vE '^/usr/lib/(libSystem\.B|libiconv\.2)\.dylib$' || true)
[ -z "$libs" ] || { echo "links more than libSystem and libiconv:"; echo "$libs"; exit 1; }
newer=$(nm -u "$bin" | grep -E 'getentropy|clock_gettime|os_unfair_lock|ulock' || true)
[ -z "$newer" ] || { echo "uses symbols newer than 10.7:"; echo "$newer"; exit 1; }
# The oldest Macs that run Lion (Core 2 Duo "Merom", 2006-2007) stop at SSSE3. rustc's default for this target is
# Penryn, which adds SSE4.1, and such a binary dies there with "Illegal instruction: 4"; builds need
# `-C target-cpu=core2` and a std rebuilt with it (release.yml). This lists SSE4.1, SSE4.2 and POPCNT instructions
# per function, leaving out code that checks the CPU before it runs (ALLOWED). Needs symbols (no `strip`).
sse4='^(pblendvb|blendvp[sd]|blendp[sd]|pblendw|ptest|pmulld|pmuldq|pcmpeqq|pcmpgtq|packusdw|pmin(sb|sd|ud|uw)|pmax(sb|sd|ud|uw)|pmov[sz]x[bwd][wdq]|pextr[bdq]|pinsr[bdq]|extractps|insertps|round[sp][sd]|dpp[sd]|mpsadbw|phminposuw|movntdqa|pcmp[ei]str[im]|crc32[bwlq]?|popcnt[wlq]?)$'
# Dispatched at run time: sha2 (SHA-NI), crc32fast (PCLMULQDQ), ring's AES-NI and SSE4.1 assembly
# (process_extra_in_trailer is a label inside the latter).
ALLOWED='^(__ZN4sha26sha2563x86|__ZN9crc32fast11specialized9pclmulqdq|_ring_core_[0-9_]+__(aes_hw_|chacha20_poly1305_(open|seal)_sse41)|process_extra_in_trailer:)'
found=$(otool -tvV "$bin" | awk -F'\t' -v re="$sse4" -v allowed="$ALLOWED" '
  NF == 1 && /:$/ { fn = $1; next }
  NF >= 2 && $2 ~ re && fn !~ allowed { n[fn]++ }
  END { for (f in n) print n[f] "\t" f }')
[ -z "$found" ] || { echo "SSE4 instructions, which Merom Core 2 Duos lack (count, function):"; echo "$found" | sort -k2 | head -50; exit 1; }
echo "sse4=none outside CPU-checked code"
