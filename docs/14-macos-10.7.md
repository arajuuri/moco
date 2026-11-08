# macOS 10.7 (Lion) support

moco supports **macOS 10.7 (Lion) through 10.14 (Mojave), Intel x86_64**, with one binary built for a 10.7 deployment
target. This was built on the `macos-10.7` branch and merged into `main` (#82). This file records what supporting 10.7
changed and why, how to build, and what still needs checking on a real Lion machine.

## Why Rust 1.78

Rust 1.74 raised the minimum macOS for `std` and the compiler to 10.12. The compiler itself only has to run on the
build machine, but programs it builds inherit the minimum through what `std` imports. #82 therefore pinned 1.73.0, the
last toolchain whose `std` imports nothing newer than 10.7.

Since #100 moco pins **1.78.0**, the newest version that every build machine can run: rustup on Linux and Mojave,
and MacPorts' `rust` port on Lion. Measured with a test program (#99), official 1.78 still writes a 10.7 minimum into
the binary but its `std` imports two functions Lion lacks, `getentropy` and `clock_gettime`.
`crates/moco/src/libc_shim.rs` defines both in the binary, so the linker binds `std`'s calls there and nothing is
imported. On 10.12+ the shim forwards to the system's functions; on Lion it falls back to `/dev/urandom` and
`gettimeofday`/`mach_absolute_time`. Newer Rust needs more (`dirfd`, `CCRandomGenerateBytes`, and a forced 10.12
minimum from about 1.86); #99 has the measurements.

## What changed

| Area | Change | Reason |
|---|---|---|
| Toolchain | `rust-toolchain.toml` and CI pin 1.78.0; `rust-version = "1.78"` in the workspace | Newest Rust every build machine runs (#100); 1.73 before that |
| libc shim | `crates/moco/src/libc_shim.rs`: `getentropy`, `clock_gettime` | 1.78's `std` imports them; both need 10.12+ |
| `Cargo.lock` | Format version 3 | Cargo before 1.78 cannot read version 4; kept while dependencies are held back |
| ratatui / crossterm | 0.30 / 0.29 down to 0.25 / 0.27 | ratatui 0.26+ needs Rust 1.74 (no longer a limit since #100; not yet upgraded) |
| rusqlite | 0.41 down to 0.31 | 0.32+ uses C string literals (1.77); some versions under-declare their MSRV |
| ignore / globset | Capped at 0.4.23 / 0.4.15 | Newer releases are edition 2024 and declare no MSRV |
| getrandom (via ring) | Pinned to 0.2.10 | 0.2.11+ calls `getentropy()` directly. That symbol exists only on 10.12+, so dyld would refuse to start the binary on Lion. 0.2.10 looks it up at run time and falls back to `/dev/urandom` |
| std back-ports | `crates/moco-core/src/compat.rs`: `Option::is_none_or`, unsigned `is_multiple_of` | Methods added after 1.78 (the 1.73 back-ports of `inspect`, `inspect_err` and `File::set_modified` went with #100) |
| ratatui back-ports | `crates/moco/src/tui/compat.rs`: `Frame::area`/`set_cursor_position`, `Layout::vertical`/`horizontal`, `Block::bordered`/`title_bottom`, `Line::style` | Methods added after 0.25 |
| Small edits | `AtomicU64::try_update` to `fetch_update`, `iter::repeat_n` to `repeat().take()`, `Buffer[(x, y)]` to `Buffer::get(x, y)` (tests), `Set<'static>` to `Set` | No back-port possible or needed |
| Lints | `unstable_name_collisions`, four clippy 1.73 lints and three clippy 1.78 lints that newer clippy does not raise on this code are allowed at crate level | Keeps code written for newer clippy unchanged |
| `scripts/install.sh` | Deployment target 10.7; minos check accepts 10.7 or older | |

The back-port traits keep call sites written in the newer style, so dropping 10.7 later means deleting the back-ports. Files that use them start with
`use crate::compat::*;` (front end: `use moco_core::compat::*;`, plus `use super::compat::*;` in `tui/`).

## Building

The supported build is Rust 1.78 through rustup on a newer Intel Mac (Mojave with the Xcode 11 Command Line Tools
works), with the binary copied to Lion. A clean release build on the Mojave test VM takes 19m22s with 1 vCPU and
9m24s with 2 (2026-11-08):

```
rustup toolchain install 1.78.0          # rust-toolchain.toml selects it inside the repo
MACOSX_DEPLOYMENT_TARGET=10.7 CARGO_TARGET_DIR=$HOME/moco-target-107 cargo build --release --locked
# In a copy without .git, also pass MOCO_GIT_DESCRIBE (CONTRIBUTING.md "Versions and releases"), or the
# version reads 0.1.0-unknown.
otool -l $HOME/moco-target-107/release/moco | grep -A3 -E 'LC_VERSION_MIN_MACOSX|LC_BUILD_VERSION'   # must say 10.7
```

Before copying the binary, also check that it links nothing newer than Lion has. `otool -L` should list only
`libSystem` and `libiconv`. `nm -u <binary> | grep -E 'getentropy|clock_gettime|os_unfair_lock|ulock'` should print
nothing (all of those need 10.12+).

### Building on Lion with MacPorts

rustup can't run on Lion (`rustup-init` needs 10.12+, and Lion's system curl can't negotiate TLS 1.2), but MacPorts
builds a toolchain from source that can. On the Lion test machine (2026-11-08) these ports are installed: `rust`
1.78.0, `cargo` 0.79.0, `clang-16`, `legacy-support`, `cctools` and `ld64`. Unchanged moco source builds with them:

```
export PATH=/opt/local/bin:/opt/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin
export MACOSX_DEPLOYMENT_TARGET=10.7 CARGO_TARGET_DIR=$HOME/moco-target-lion
export CC=clang-mp-16 CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=clang-mp-16
# MacPorts' Rust std calls functions newer than 10.7 (getentropy, ...); MacPorts' static library supplies them.
export RUSTFLAGS="-C link-arg=-Wl,/opt/local/lib/libMacportsLegacySupport.a"
cargo build --release --locked     # pass MOCO_GIT_DESCRIBE too in a copy without .git
```

A clean release build takes about 15 minutes on the 1-vCPU Lion VM (15m17s before #100, 15m41s with it). The result
passes the same checks as the Mojave build: `LC_VERSION_MIN_MACOSX` 10.7, `otool -L` lists only `libSystem` and
`libiconv`, and the `nm -u` check prints nothing.

`libMacportsLegacySupport.a` is still needed with moco's own shim: proc macros and build scripts are linked against
std too and run on Lion during the build, and the shim is only in the `moco` binary. Without the library the build
stops at the first proc macro (`zerofrom-derive`: undefined `_getentropy`). With both, the binary uses the shim's
definitions and nothing clashes.

Without rustup, `rust-toolchain.toml` is ignored. Since #100 the pin is MacPorts' version, 1.78.0, so a Lion build
uses the pinned compiler. MacPorts keeps Rust frozen at 1.78.0 for 10.11 and older; its newer releases need 10.12, so
1.78 is the newest Rust that builds on Lion. The Lion test VM doesn't boot with 2 vCPUs (tried 2026-11-08), so it stays at 1.

## Regenerating the lockfile

Use a newer cargo with the MSRV-aware fallback resolver, then convert the lockfile back to version 3:

```
CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback cargo +stable update
sed -i '' 's/^version = 4$/version = 3/' Cargo.lock   # GNU sed: sed -i
cargo +1.78.0 build --locked && cargo +1.78.0 test --locked
```

If a crate fails on 1.78 despite the resolver, it is under-declaring its `rust-version`. Cap it in `Cargo.toml` with a
comment saying why.

## Writing code for 1.78

Code and examples written for newer Rust or ratatui may use APIs newer than 1.78 / 0.25. Prefer adding a back-port to the relevant `compat.rs` over editing
the call site, then run `cargo build`, `cargo test` and `cargo clippy --workspace --all-targets -- -D warnings`.

## Verified, and not yet verified

Verified with Rust 1.78.0 and the libc shim (#100, 2026-11-08):
- Linux: clippy `-D warnings` passes and `cargo test` passes (604 tests).
- Mojave 10.14.6, official 1.78.0 from rustup: clippy passes, `cargo test` passes (607 tests, including the shim's
  fallback tests), and a clean release build takes 7m44s with 2 vCPUs. The binary has `LC_VERSION_MIN_MACOSX` 10.7,
  links only `libSystem` and `libiconv`, defines `getentropy` and `clock_gettime` itself, and every symbol it imports
  is exported by Lion's system libraries (checked against all 33,886 of them, extracted with `nm -gU`).
- Lion 10.7.5 with that binary: `--version`, `models --refresh` (HTTPS), `-p` against the Anthropic API (dummy key,
  `invalid x-api-key`) and `sessions` work. Those paths run the shim's fallbacks: std seeds `HashMap` keys with
  `getentropy` and reads its clocks with `clock_gettime`.
- Lion with MacPorts 1.78: the release build works (see "Building on Lion with MacPorts"). `cargo test` fails the same
  9 `moco-core` tests as before #100 (#97, #98) and passes the rest (603), including the shim tests, which on Lion run
  the fallbacks.

The rest of this section was verified with Rust 1.73.0, before #100.

Verified on Linux with Rust 1.73.0:
- The workspace builds.
- `cargo test` passes: 584 tests across both crates, including the TUI buffer and pty tests.
- clippy passes with `-D warnings`.
- The macOS-only code compiles there too, because it is gated with `cfg!` rather than `#[cfg]`, apart from one sandbox test.

Verified with a release build made on Mojave 10.14.6 (Apple clang 11.0.0, 10.14 SDK, `MACOSX_DEPLOYMENT_TARGET=10.7`):
- The 10.14 SDK links for 10.7 without problems; `crt1.10.6.o` is still in the SDK.
- `otool -l` reports `LC_VERSION_MIN_MACOSX` version 10.7. `otool -L` lists only `libSystem` and `libiconv`.
- Every one of the binary's 207 undefined symbols (`nm -u`) is exported by Lion's `/usr/lib/system/*.dylib`,
  `libSystem` or `libiconv`, so the bundled SQLite and ring use nothing newer than 10.7.

Verified on Lion 10.7.5 (11G63) with that binary:
- It starts: `--version`, `--help`, `models` and `sessions` work, and the SQLite store opens and migrates.
- HTTPS works: `moco models --refresh` downloads the catalog, and `-p` reaches the Anthropic API (checked with a dummy
  key, which gets `invalid x-api-key`). TLS is rustls/ring with the roots compiled in (`webpki-roots`), so Lion's old
  keychain and system TLS do not matter.
- moco's exact Seatbelt profile works under Lion's `sandbox-exec`, `(remote unix-socket)` included: writes outside the
  allowed subpaths are refused, `/tmp` is writable, and outbound network is blocked.
- The TUI over an SSH pty (`TERM=xterm-256color`) draws the header, input box and spinner with Unicode box-drawing
  glyphs, sends a message, shows the error and exits cleanly.
- A real session with a valid Anthropic key (2026-11-08, TUI in auto mode): it read and edited a file, ran
  `python3 -m unittest` through the shell tool, and showed the suggested next message after each turn.

The same binary also runs on Mojave 10.14.6 (`--version`, `models --refresh`, `-p` against the API). macOS 10.8 to
10.13 should work for the same reasons (minimum OS 10.7, nothing newer linked, OS-independent TLS) but are untested.

**Not yet verified:**
- Lion's Terminal.app itself, as opposed to an SSH pty: true colour (`Color::Rgb`) themes and the glyphs (`MOCO_ASCII=1`
  is the fallback).
- A clean `cargo test` on Lion. With the MacPorts toolchain ("Building on Lion with MacPorts"), `cargo test
  --workspace` (2026-11-08, 11 min at 1 vCPU) passes 597 tests and fails 9, all in `moco-core`:
  - Seven snapshot, undo, diff and prune tests fail because Lion's disk is HFS+, which keeps mtimes to the whole
    second (APFS keeps nanoseconds). Six of them show a real gap (issue #97): snapshots reuse a file's hash when
    its size and mtime are unchanged, so an edit that keeps the size and lands in the same second as the previous
    snapshot is missed by undo and the session diff. This affects any HFS+ volume, not only Lion. The seventh
    assumes sub-second mtimes in its own timing (issue #98).
  - Two background-job tests fail because they check for the process with `pgrep`, which Lion lacks (issue #98).
- macOS 10.7.0 to 10.7.4 and 10.8 to 10.13.
- `sw_vers` reports "Mac OS X 10.7.x", while `sysinfo` labels it "macOS 10.7.x". This is cosmetic.

Running this build and a `main` build against the same data directory is not safe: `Store::init` only applies
migrations newer than the recorded one and does not refuse a schema newer than it knows. Give each build its own
`MOCO_HOME` if both are installed on one machine.
