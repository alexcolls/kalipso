# Using kalipso-cli

```sh
kalipso-cli path/to/project              # defaults to the current folder
cargo run --release -- path/to/project   # or from source
```

**Ctrl+Q** quits. With unsaved changes or a running agent it asks first: `y` quits, Esc stays.

To chat with an agent, press Alt+4 and Enter, and pick one. Claude Code, Codex and Antigravity sign in
with your own plan ([details](agents.md#claude-code-codex-and-antigravity)); kalipso-cli's own agents
need a key from [AI providers](agents.md#providers), set before you start.

## Layout

Four panes: **1 Files**, **2 Terminal**, **3 Editor**, **4 Chat**. Each pane can hold several tabs.
A click also focuses a pane. The status line always lists the focused pane's keys.

## Global keys

These work everywhere, including inside the terminal pane.

| Key | Action |
|---|---|
| Alt+←/→ | Files ↔ Editor/Terminal ↔ Chat |
| Alt+↑/↓ | Editor ↕ Terminal |
| Alt+1/2/3/4 | Focus Files / Terminal / Editor / Chat |
| Alt+Z | Zoom the focused pane to full screen and back |
| Alt+T / Alt+W | New / close a tab in the focused pane |
| Alt+PgUp / Alt+PgDn | Previous / next tab |
| Ctrl+Q | Quit |

If these don't arrive, see [Troubleshooting](troubleshooting.md).

## Finding things

**Ctrl+P** finds a file by fuzzy name and **Alt+F** searches the project's text, from anywhere except
the terminal pane, where those keys belong to the shell. Search skips gitignored and private files.

## Files pane

| Key | Action |
|---|---|
| Enter | Open |
| ←/→ | Fold / unfold |
| `n` / `N` | New file / folder |
| `r` | Rename |
| `d` or Delete | Delete |
| `.` | Show hidden and ignored files |
| `R` | Refresh |

In a git repository, `M` marks changed files, `A` new ones, and `•` folders with changes inside.

## Editor

VS Code-style.

| Key | Action |
|---|---|
| Ctrl+S | Save |
| Ctrl+Z / Ctrl+Y | Undo / redo |
| Ctrl+C / X / V | Clipboard |
| Ctrl+A | Select all |
| Ctrl+F, F3 / Shift+F3 | Find, next / previous match |
| Ctrl+G | Go to line |
| Ctrl+R | Reload from disk |
| Shift+motion | Select |
| F12 / Shift+F12 | Go to definition / back ([language servers](language-servers.md)) |
| F8 | Next problem |

Ctrl+V pastes what you copied in kalipso-cli; use your terminal's paste (usually Ctrl+Shift+V, or
Cmd+V on macOS) for text from other programs.

The gutter marks lines that differ from git's HEAD: green added, yellow changed, red for deleted
lines. Binary files, files over 64 MB and anything that isn't a regular file won't open.

## Terminal pane

Every other key goes to the shell. Alt+S enters scroll mode, where Esc or `q` leaves and the wheel
scrolls.

## Chat pane

Alt+T starts a chat, as does Enter when none is open. Pick Claude Code, Codex or Antigravity to run
that vendor's CLI with your own plan, or one of kalipso-cli's agents, then a provider if you've set
several keys. In an agent's chat, Enter sends, Ctrl+J adds a line, Esc stops the agent, PgUp/PgDn
scroll.

| Command | Effect |
|---|---|
| `/model` | Switch model within the provider |
| `/compact` | Summarize the conversation now |
| `/copy` | Copy the last reply |

More in [AI providers and agents](agents.md).

## Copying

Drag in the terminal or chat pane to copy what's on screen, or use `/copy` for the whole last reply.
kalipso-cli captures the mouse, so hold Shift for your terminal's own selection, which runs across all
panes.

## Not built yet

Completion, hover and rename from language servers, editor splits, multi-cursor, custom agents, MCP,
a settings UI, in-app API key entry, mouse forwarding to programs in the terminal pane, full Markdown
rendering, and a Vim mode.
