# moco

A native coding agent for the terminal on **macOS 10.7 (Lion) through 10.14 (Mojave)**, built from a clean-room spec of opencode. It runs on Intel Macs with nothing but the system libraries (a single Rust binary built with Rust 1.78 and `MACOSX_DEPLOYMENT_TARGET=10.7`; see [docs/14-macos-10.7.md](docs/14-macos-10.7.md)) and works headless over SSH.

Status: Phases 0-2 are implemented and verified on a real Mojave machine: streaming chat with tool calls (read, write, edit, patch, glob, grep, shell, webfetch, todos, questions, subagents), permissions, Anthropic and OpenAI-compatible providers, a models.dev catalog with cost tracking, prompt caching, context compaction, `AGENTS.md`, JSONC config, undo/redo/diff, and session list/resume/fork/archive. Skills, custom commands, MCP and background jobs are Phase 3.

- Using it: `docs/11-usage.md`
- Design and specification: `docs/00-overview.md`; build order, results and what is left: `docs/10-roadmap.md`
- Working on it: `AGENTS.md`

Upstream reference: opencode at commit `6c24ec9532`. Source paths cited in the docs are relative to that checkout.
