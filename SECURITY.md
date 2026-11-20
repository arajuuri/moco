# Security policy

## Reporting a vulnerability

Please report security problems privately, not in a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/arajuuri/moco/security/advisories/new)
("Report a vulnerability" on the repository's Security tab).

Include the moco version (`moco --version`), your macOS version, and the steps to reproduce.
I'll acknowledge a report within a week and keep you updated until it is fixed. moco is
maintained by one person, so fix times vary with the severity. If you want credit, say how you
would like to be named in the release notes.

## Supported versions

Only the [latest release](https://github.com/arajuuri/moco/releases/latest) gets fixes.

## What counts

moco runs shell commands and edits files on your machine on a model's behalf, so the
interesting problems are the ones that let it do more than you allowed. For example:

- bypassing the permission system, the `.env` protection, or the project-command approval for
  MCP servers, formatters and the `shell` executable
- escaping the Seatbelt sandbox
- leaking provider API keys or stored credentials (to shell commands, logs, exports, other
  servers or the model)
- a crafted repository, file, web page or MCP server that makes moco run commands or write
  files without the approval its settings require
- path traversal or symlink tricks that read or write outside the allowed directories

Not security bugs: behavior that follows from the mode you chose (auto mode runs tools
without asking, and is not a security boundary), a model doing something unwise inside the
permissions you granted, and issues that need an attacker who already controls your account
or your `~/Library/Application Support/moco` directory.

macOS 10.7 through 10.14 no longer receive Apple security updates. moco supports them on
purpose, but the operating system's own vulnerabilities are out of scope.
