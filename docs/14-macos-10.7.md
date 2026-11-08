# macOS 10.7 (Lion) branch

The `macos-10.7` branch builds moco for **macOS 10.7 (Lion), Intel x86_64**. `main` targets 10.14. This file records what
the branch changes and why, how to build it, and what still needs checking on a real Lion machine.

## Why it needs a branch

Rust 1.74 raised the minimum macOS for `std` and the compiler to 10.12, so **Rust 1.73.0 is the last toolchain that
targets 10.7**. `main` pins a much newer Rust and dependencies that need it, so the branch pins 1.73 and works backwards
from there.

## What changed

| Area | Change | Reason |
|---|---|---|
| Toolchain | `rust-toolchain.toml` and CI pin 1.73.0; `rust-version = "1.73"` in the workspace | Last Rust for 10.7 |
| `Cargo.lock` | Format version 3 | Cargo 1.73 cannot read version 4 |
| ratatui / crossterm | 0.30 / 0.29 down to 0.25 / 0.27 | ratatui 0.26+ needs Rust 1.74 |
| rusqlite | 0.41 down to 0.31 | 0.32+ uses C string literals (1.77); some versions under-declare their MSRV |
| ignore / globset | Capped at 0.4.23 / 0.4.15 | Newer releases are edition 2024 and declare no MSRV |
| getrandom (via ring) | Pinned to 0.2.10 | 0.2.11+ calls `getentropy()` directly. That symbol exists only on 10.12+, so dyld would refuse to start the binary on Lion. 0.2.10 looks it up at run time and falls back to `/dev/urandom` |
| std back-ports | `crates/moco-core/src/compat.rs`: `Option::is_none_or`, `Option`/`Result::inspect`, `Result::inspect_err`, unsigned `is_multiple_of`, `File::set_modified` (via `futimes`; `futimens` needs 10.13) | Methods added after 1.73 |
| ratatui back-ports | `crates/moco/src/tui/compat.rs`: `Frame::area`/`set_cursor_position`, `Layout::vertical`/`horizontal`, `Block::bordered`/`title_bottom`, `Line::style` | Methods added after 0.25 |
| Small edits | `AtomicU64::try_update` to `fetch_update`, `iter::repeat_n` to `repeat().take()`, `Buffer[(x, y)]` to `Buffer::get(x, y)` (tests), `Set<'static>` to `Set` | No back-port possible or needed |
| Lints | `unstable_name_collisions` and four clippy 1.73 lints that later clippy dropped or made opt-in are allowed at crate level | Keeps the code identical to `main` |
| `scripts/install.sh` | Deployment target 10.7; minos check accepts 10.7 or older | |

The back-port traits keep call sites identical to `main`, which keeps merges small. Files that use them start with
`use crate::compat::*;` (front end: `use moco_core::compat::*;`, plus `use super::compat::*;` in `tui/`).

## Building

Lion can't build moco itself: rustup and current `rustup-init` binaries need 10.12+, and Lion's system curl can't
negotiate TLS 1.2. Build on a newer Intel Mac (Mojave with the Xcode 11 Command Line Tools works; a release
build takes about 15 minutes there) and copy the binary
over:

```
rustup toolchain install 1.73.0          # rust-toolchain.toml selects it inside the repo
MACOSX_DEPLOYMENT_TARGET=10.7 CARGO_TARGET_DIR=$HOME/moco-target-107 cargo build --release --locked
otool -l $HOME/moco-target-107/release/moco | grep -A3 -E 'LC_VERSION_MIN_MACOSX|LC_BUILD_VERSION'   # must say 10.7
```

Before copying the binary, also check that it links nothing newer than Lion has. `otool -L` should list only
`libSystem` and `libiconv`. `nm -u <binary> | grep -E 'getentropy|clock_gettime|os_unfair_lock|ulock'` should print
nothing (all of those need 10.12+).

## Regenerating the lockfile

Use a newer cargo with the MSRV-aware fallback resolver, then convert the lockfile back to version 3:

```
CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback cargo +stable update
sed -i '' 's/^version = 4$/version = 3/' Cargo.lock   # GNU sed: sed -i
cargo +1.73.0 build --locked && cargo +1.73.0 test --locked
```

If a crate fails on 1.73 despite the resolver, it is under-declaring its `rust-version`. Cap it in `Cargo.toml` with a
comment saying why.

## Merging `main`

Expect conflicts in `Cargo.toml`, `Cargo.lock` and `rust-toolchain.toml`: keep this branch's side. New code from `main`
may use std or ratatui APIs newer than 1.73 / 0.25. Prefer adding a back-port to the relevant `compat.rs` over editing
the call site, then run `cargo build`, `cargo test` and `cargo clippy --workspace --all-targets -- -D warnings`.

## Verified, and not yet verified

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

The same binary also runs on Mojave 10.14.6 (`--version`, `models --refresh`, `-p` against the API). macOS 10.8 to
10.13 should work for the same reasons (minimum OS 10.7, nothing newer linked, OS-independent TLS) but are untested.

**Not yet verified:**
- A real session with a valid API key that runs tools on Lion.
- Lion's Terminal.app itself, as opposed to an SSH pty: true colour (`Color::Rgb`) themes and the glyphs (`MOCO_ASCII=1`
  is the fallback).
- `cargo test` on Lion: the fake LLM/MCP servers need `python3`, and Lion has only Python 2.7.
- macOS 10.7.0 to 10.7.4 and 10.8 to 10.13.
- `sw_vers` reports "Mac OS X 10.7.x", while `sysinfo` labels it "macOS 10.7.x". This is cosmetic.

Running this build and a `main` build against the same data directory is not safe: `Store::init` only applies
migrations newer than the recorded one and does not refuse a schema newer than it knows. Give each build its own
`MOCO_HOME` if both are installed on one machine.
