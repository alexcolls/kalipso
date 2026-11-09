# Kalipso documentation

Kalipso is two separate programs that work well together.

| Program | What it is | Platforms |
|---|---|---|
| **Kalipso** | A desktop terminal: [WezTerm](https://wezterm.org) with a curated configuration, ten live-switchable themes and FiraCode Nerd Font. Nothing else to install. | Linux x86_64, Apple silicon macOS, Windows x86_64 |
| **kalipso-cli** | A VS Code/Cursor-style IDE that runs entirely in a terminal: project files, multiple shells, an editor and multi-chat with AI agents. | Linux x86_64, Apple silicon macOS, Windows through WSL |

```
┌ 1 Files ┬ 3 Editor ────────┬ 4 Chat ──┐
│         │                  │          │
│         ├ 2 Terminal ──────┤          │
│         │                  │          │
└─────────┴──────────────────┴──────────┘
```

## Where to go

| If you want to... | Read |
|---|---|
| Install either program | [Installation](installation.md) |
| Know what the desktop program does and how to remove it | [Kalipso desktop](desktop.md) |
| Learn the panes, keys, editor and search | [Using kalipso-cli](cli.md) |
| Chat with Claude, OpenAI, Grok or OpenRouter agents | [AI providers and agents](agents.md) |
| Get go-to-definition and diagnostics | [Language servers](language-servers.md) |
| Fix Alt keys, tmux, WezTerm or Windows problems | [Troubleshooting](troubleshooting.md) |
| Build, test and release | [Development](development.md) |

## Platform status

| | Linux | macOS | Windows |
|---|---|---|---|
| Kalipso | tested | not run by hand | not run by hand |
| kalipso-cli | tested | test suite passes, not run by hand | WSL only |
