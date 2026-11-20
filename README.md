# moco

A coding agent for the terminal that runs on old Intel Macs: **macOS 10.7 (Lion) through 10.14 (Mojave)**. It is a single Rust binary with no runtime besides the system libraries, so it also works headless over SSH. moco is a clean-room reimplementation of [opencode](https://github.com/anomalyco/opencode), built from a written specification of it (see [NOTICE](NOTICE)).

Most current coding agents need a runtime (Node, Bun, Electron) that these systems cannot load. moco is built with Rust 1.78 and `MACOSX_DEPLOYMENT_TARGET=10.7`, and the same binary runs on every release in that range. Details are in [docs/14-macos-10.7.md](docs/14-macos-10.7.md).

> **Disclaimer: this software was written by AI and has not been reviewed by humans.**
> moco was built almost entirely by AI coding agents working from a written specification. No
> person has read the source code line by line. It has automated tests and has been run on real
> machines, but that is not a review, and it may contain bugs, security holes and unsafe behavior
> that nobody has noticed. It runs shell commands and edits files on your computer. Use it at your
> own risk, in a version-controlled directory and preferably a disposable one, and read the
> [Safety](#safety) section. There is no warranty (see [LICENSE](LICENSE)).

## Features

- Full-screen terminal UI (ratatui), a plain line chat (`--plain`), and one-shot mode (`moco -p "prompt"`)
- Tools: read, write, edit, patch, glob, grep, shell, webfetch, websearch, todos, questions, subagents, background commands
- Providers: Anthropic, OpenAI (Responses and Chat Completions), Google Gemini, OpenRouter and other OpenAI-compatible endpoints, with a [models.dev](https://models.dev) catalog and cost tracking
- Permissions: ask before acting (manual) or run with guardrails (auto), saved "always allow" rules, an optional macOS Seatbelt sandbox for shell commands
- Sessions that survive crashes: resume, fork, undo/redo with a diff of the working tree, context compaction, prompt caching
- Extensibility: `AGENTS.md`, skills, custom slash commands, `@` mentions, MCP servers (stdio; Streamable HTTP and OAuth are experimental), formatter hooks

## Install

Short version, once you have downloaded and unpacked the [latest release](https://github.com/arajuuri/moco/releases/latest) (Intel x86_64):

```
sudo mkdir -p /usr/local/bin
sudo cp moco-*-macos-x86_64/moco /usr/local/bin/moco
moco --version
```

Step-by-step instructions, including checking the download, a no-`sudo` option, Lion's download limits, the macOS "unidentified developer" prompt, updating and uninstalling, are in **[INSTALL.md](INSTALL.md)**. To build from source instead, see `scripts/install.sh` and [docs/11-usage.md](docs/11-usage.md).

## Quick start

```
moco auth anthropic           # or set ANTHROPIC_API_KEY
cd your-project
moco                          # interactive chat
moco -p "explain this repo"   # one request, then exit
```

With OpenAI or Google instead, store the key with `moco auth openai` or `moco auth google` and name a model: `moco --model openai/MODEL` or `moco --model google/MODEL` (`moco models` lists them). [INSTALL.md](INSTALL.md) has the details.

The [usage guide](docs/11-usage.md) covers commands, configuration, permissions, skills and MCP.

## Safety

Because no human has reviewed the code, treat every safeguard below as untested by a person: the permission system, the sandbox and the secret-handling rules were all written and checked by the same AI that wrote the rest.

moco runs shell commands and edits files on your machine. The default permission mode asks before each action. Auto mode runs tools without asking and applies a "careful" rule preset plus a model-based classifier for routine shell commands, but it is not a security boundary. Review what it does, work in a version-controlled directory, and consider enabling the sandbox. Provider API keys are not passed to shell commands, MCP servers or formatters unless you say so.

## Status

moco is young and moves quickly. The Anthropic, OpenAI and Gemini providers have been verified against the live APIs. Remote MCP (Streamable HTTP and OAuth) and the Brave, Tavily, Exa and SearXNG search providers have so far only been tested against fakes. AWS Bedrock, Google Vertex AI and GitHub Copilot are not supported yet. The open [issues](https://github.com/arajuuri/moco/issues) list what is missing. It is developed and tested on macOS 10.7 and 10.14; CI runs the test suite on Linux.

## Documentation

- [USER-GUIDE.md](USER-GUIDE.md): a step-by-step manual for non-technical users
- [INSTALL.md](INSTALL.md): installing moco
- [docs/11-usage.md](docs/11-usage.md): using moco
- [docs/00-overview.md](docs/00-overview.md): design and specification
- [docs/13-architecture.md](docs/13-architecture.md): the code as built
- [CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md): working on moco

## License

MIT, see [LICENSE](LICENSE). The specifications in `docs/` derive from opencode (MIT), see [NOTICE](NOTICE) and [LICENSE-opencode](LICENSE-opencode).
