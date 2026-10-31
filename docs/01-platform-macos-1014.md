# Platform constraints: macOS 10.14

Caveat: this is from my own knowledge of Apple/Rust/Swift release history, not verified in this session. Re-check against a
real Mojave machine or VM before committing to a toolchain. The facts from the repo (Bun/Electron minimums) were measured.

## What 10.14 rules out

| Technology | Why it is a problem |
|---|---|
| SwiftUI, Combine | Introduced in 10.15 |
| Swift concurrency (`async`/`await`) | Back-deploys only to 10.15+ |
| Bun, current Node, Electron 23+ | Minimum OS above 10.14 (measured for Bun) |
| Swift on 10.14 | Needs Swift 5 runtime, present from 10.14.4. Target 10.14.4 minimum, not 10.14.0 |
| Newest Xcode on the Mac itself | Last Xcode that runs on Mojave is 11.x; you can build on a newer Mac with deployment target 10.14 instead |

## What works

- **AppKit + Foundation + URLSession + Network.framework** (10.14 has Network.framework).
- **SQLite**: system `libsqlite3` (version is old; avoid newer features like `RETURNING` if relying on system lib) or a bundled amalgamation (preferred; pin a version).
- **JSON**: `Codable`. **Process spawning**: `Process`/`posix_spawn`. **PTY**: `forkpty` from libutil.
- **TLS**: URLSession works with TLS 1.2; TLS 1.3 support in URLSession on 10.14 is uncertain. Anthropic, OpenAI and Google endpoints accept TLS 1.2 today, but verify. Alternatively bundle a TLS stack (rustls via Rust).
- **Root certificates**: 10.14 system trust store is frozen at the OS's last update; new CAs (for example Let's Encrypt chain changes) can fail. Provide an option to load a PEM bundle.
- **Concurrency**: GCD (`DispatchQueue`), `OperationQueue`, delegate-based `URLSessionDataDelegate` for streaming.

## Toolchain options

| Option | Pros | Cons |
|---|---|---|
| **A. Swift + AppKit** (recommended for a GUI app) | Native look, Keychain, drag-drop, no runtime to ship. Deployment target 10.14.4 works with Xcode 11-15 | No async/await or Combine; callback-style code. Terminal view: use SwiftTerm if its deployment target allows (check), else write a minimal VT100 view |
| **B. Rust core + thin AppKit shell or TUI (ratatui)** | One portable engine, tokio streaming, rustls, `rusqlite` bundled. Rust's x86_64-apple-darwin target historically floors at macOS 10.12 | Larger FFI surface if you want a Cocoa UI; verify current floor of the Rust version you use |
| **C. Go** | Simple static binary | Go 1.21+ requires macOS 10.15 or newer; only Go 1.20 and older reach 10.14. Not recommended |
| **D. Python/Node runtime bundled** | Fast to write | Needs an interpreter built for 10.14 (Node 16 or Python 3.8-3.9). Old, unpatched, large. Not recommended |

**Recommendation:** B for fastest path to a working agent that is also testable on Linux CI (engine + TUI), then add an AppKit GUI later if wanted.
Choose A if the product is explicitly a Mac GUI app and you accept callback-style code.

## Hardware note

macOS 10.14 means Intel Macs only (no Apple silicon). Build a single `x86_64` binary with `MACOSX_DEPLOYMENT_TARGET=10.14`.
Verify the result: `otool -l <binary> | grep -A3 LC_BUILD_VERSION` must show `minos 10.14`, and check every dylib you link with
the same command (the repo's own `script/native-arch` style verification is a good model: fail the build if any Mach-O has a higher minos).

## Subprocess tools available on the Mac

The tools depend on external programs. On 10.14 you get `/bin/zsh`, `/bin/bash` 3.2, system `git` only if Xcode CLT is installed,
no `rg`. Bundle ripgrep (an x86_64 build with minos <= 10.14) or implement grep natively. Upstream uses ripgrep for `grep` and `glob`
(`core/src/ripgrep.ts`).
