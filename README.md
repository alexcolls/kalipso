<div align="center">

# kalipso

**Futuristic open-source terminal.**

[![CI](https://github.com/alexcolls/kalipso/actions/workflows/ci.yml/badge.svg)](https://github.com/alexcolls/kalipso/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/alexcolls/kalipso)](https://github.com/alexcolls/kalipso/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[Documentation](https://alexcolls.github.io/kalipso/) ·
[Installation](docs/installation.md) ·
[Releases](https://github.com/alexcolls/kalipso/releases)

</div>

Kalipso is two separate programs that work well together:

| | **Kalipso** | **kalipso-cli** |
|---|---|---|
| **What** | Desktop terminal: [WezTerm](https://wezterm.org) with [a curated config](https://github.com/alexcolls/wezterm-config), ten live-switchable themes and FiraCode Nerd Font | VS Code/Cursor-style IDE that runs entirely in a terminal |
| **Includes** | Nothing else to install | Project files, multiple shells, an editor, git gutter, language servers, multi-chat with AI agents |
| **AI** | n/a | Claude, OpenAI, Grok and OpenRouter |
| **Platforms** | Linux x86_64, Apple silicon macOS, Windows x86_64 | Linux x86_64, Apple silicon macOS, Windows via WSL |

```
┌ 1 Files ┬ 3 Editor ────────┬ 4 Chat ──┐
│         │                  │          │
│         ├ 2 Terminal ──────┤          │
│         │                  │          │
└─────────┴──────────────────┴──────────┘
```

## Quick start

**Kalipso (desktop)**

```sh
# Linux x86_64, Apple silicon macOS
curl -fsSL https://raw.githubusercontent.com/alexcolls/kalipso/main/install.sh | sh
```

```powershell
# Windows x86_64, PowerShell
irm https://raw.githubusercontent.com/alexcolls/kalipso/main/install.ps1 | iex
```

**kalipso-cli (terminal IDE)**

```sh
curl -fsSL https://raw.githubusercontent.com/alexcolls/kalipso/main/install-cli.sh | sh
export ANTHROPIC_API_KEY=...     # or OPENAI_API_KEY, XAI_API_KEY, OPENROUTER_API_KEY
kalipso-cli path/to/project
```

Run an installer again to update. Press **Ctrl+Q** to quit, **Alt+4** then Enter to start a chat.

## Highlights

- **Four-pane workspace** with tabs: files, terminal, editor and chat, driven by Alt keys and the mouse.
- **Multi-chat AI agents** (Code, Test, Review, Commit, Design) with per-action approval of file writes and commands.
- **Editor** with undo/redo, find, go to line, git change gutter, and fuzzy file and text search.
- **Language servers** for Rust, Python, TypeScript, Go and C/C++: diagnostics and go to definition, started only with your consent.
- **Private by default:** keys come from environment variables only, are never written to disk, and are stripped from commands and language servers.
- **Resumable chats** with automatic compaction of long conversations.

## Documentation

Browse it at **<https://alexcolls.github.io/kalipso/>**, or read the Markdown in [`docs/`](docs/).

| Guide | Covers |
|---|---|
| [Installation](docs/installation.md) | Installers, manual and source installs, uninstalling |
| [Kalipso desktop](docs/desktop.md) | The desktop program, keys, themes, platform notes |
| [Using kalipso-cli](docs/cli.md) | Panes, every key binding, editor, search, copying |
| [AI providers and agents](docs/agents.md) | Keys, agents, long chats, safety model, saved chats |
| [Language servers](docs/language-servers.md) | Supported servers, consent, features |
| [Troubleshooting](docs/troubleshooting.md) | Alt keys on macOS, WezTerm and tmux; Windows issues |
| [Development](docs/development.md) | Layout, tests, releases, previewing the docs |

## Security

There is no sandbox. **Approval is the security boundary**: an approved command can do anything your
user account can, and files or command output may contain prompt injection. Read commands before you
allow them. Details in [AI providers and agents](docs/agents.md#safety-model).

## Status

Linux is the tested platform. macOS passes the test suite but hasn't been run by hand; Windows isn't
tested by hand either. See [Kalipso desktop](docs/desktop.md#whats-been-tried).

## Contributing

```sh
cargo test          # offline unit and integration tests
tests/e2e.sh        # drives the real binary in tmux against mock servers
```

CI also runs `cargo fmt --check`, `cargo clippy` and the installer tests. See
[Development](docs/development.md).

## License

[MIT](LICENSE)
