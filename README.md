# kalipso
>_ Futuristic open-source terminal

A VS Code/Cursor-style IDE that runs entirely in your terminal: project files, multiple
shells, an editor, and multi-chat with AI agents, on Claude, OpenAI, Grok and OpenRouter.

```
┌ 1 Files ┬ 3 Editor ────────┬ 4 Chat ──┐
│         │                  │          │
│         ├ 2 Terminal ──────┤          │
│         │                  │          │
└─────────┴──────────────────┴──────────┘
```

## Run it

```sh
cargo run --release -- path/to/project   # defaults to the current folder
```

Linux is the tested platform. Agents' `run_command` tool needs a Unix system.

## Keys

These work everywhere, including inside the terminal pane:

| Key | Action |
|---|---|
| Alt+←/→ | Files ↔ Editor/Terminal ↔ Chat |
| Alt+↑/↓ | Editor ↕ Terminal |
| Alt+1/2/3/4 | Focus Files / Terminal / Editor / Chat |
| Alt+Z | Zoom the focused pane to full screen and back |
| Alt+T / Alt+W | New / close a tab in the focused pane |
| Alt+PgUp / Alt+PgDn | Previous / next tab |
| Ctrl+Q | Quit |

A click also focuses a pane. Kalipso captures the mouse, so hold Shift to select text with
your terminal's own selection. The status line always lists the focused pane's keys.

**Files:** Enter opens, ←/→ fold, `n` new file, `N` new folder, `r` rename, `d` delete,
`.` show hidden and ignored files, `R` refresh.

**Editor:** VS Code-style. Ctrl+S save, Ctrl+Z/Y undo/redo, Ctrl+C/X/V clipboard, Ctrl+A select
all, Ctrl+F find, F3 next match, Ctrl+G go to line, Ctrl+R reload from disk, Shift+motion selects.
Ctrl+V pastes what you copied in Kalipso; use your terminal's paste (usually Ctrl+Shift+V) for
text from other programs.

**Terminal:** every other key goes to the shell. Alt+S enters scroll mode, and the wheel scrolls.

**Chat:** Enter sends, Ctrl+J adds a line, Esc stops the agent, PgUp/PgDn scroll, `/model` switches
model.

### If Alt keys don't reach Kalipso

Terminals can keep these shortcuts for themselves. Clicking always works as a fallback.

**WezTerm:** Kalipso sets a `KALIPSO` user variable while it runs, so a config can pass the keys
through only then. Wrap your existing bindings:

```lua
local act = wezterm.action

local function kalipso_or(key, fallback)
  return wezterm.action_callback(function(window, pane)
    if pane:get_user_vars().KALIPSO == '1' and pane:is_alt_screen_active() then
      window:perform_action(act.SendKey { key = key, mods = 'ALT' }, pane)
    else
      window:perform_action(fallback, pane)
    end
  end)
end

-- Do the same for the other arrows, and Alt+1-4 if you've bound them.
{ key = 'LeftArrow', mods = 'ALT', action = kalipso_or('LeftArrow', act.ActivatePaneDirection 'Left') },
```

This also works over SSH. It doesn't work through tmux, which swallows the variable.

**tmux**, if your config binds Alt+arrows:

```tmux
bind -n M-Left  if -F '#{==:#{pane_current_command},kalipso}' 'send-keys M-Left'  'select-pane -L'
bind -n M-Right if -F '#{==:#{pane_current_command},kalipso}' 'send-keys M-Right' 'select-pane -R'
bind -n M-Up    if -F '#{==:#{pane_current_command},kalipso}' 'send-keys M-Up'    'select-pane -U'
bind -n M-Down  if -F '#{==:#{pane_current_command},kalipso}' 'send-keys M-Down'  'select-pane -D'
```

## AI providers

Keys come from environment variables only. Kalipso never writes them to disk or shows them.

| Provider | Key | Base URL override |
|---|---|---|
| Claude | `ANTHROPIC_API_KEY` | `ANTHROPIC_BASE_URL` |
| OpenAI | `OPENAI_API_KEY` | `OPENAI_BASE_URL` |
| Grok | `XAI_API_KEY` | `XAI_BASE_URL` |
| OpenRouter | `OPENROUTER_API_KEY` | `OPENROUTER_BASE_URL` |

Claude needs an API key from the Claude Console; a Claude subscription doesn't provide one. Your
model choice per provider is saved in `~/.config/kalipso/config.json`. Claude defaults to
`claude-opus-5-5`; for the others you pick from the provider's list once.

For Claude models that support it, Kalipso turns on server-side fallbacks: if safety classifiers
decline a request, it's retried on a fallback model, billed at that model's rates.

A chat's agent and provider are fixed when it starts, because conversation history is stored in
each provider's own format. `/model` switches within a provider; otherwise open a new chat.

## Agents

| Agent | Tools | Job |
|---|---|---|
| Code | all | Implement changes |
| Test | all | Write and run tests, fix failures |
| Review | read + `run_command` | Review diffs and code; no file-editing tools |
| Commit | read + `run_command` | Inspect the diff, write the message, commit |
| Design | read + `write_file` | Design docs and ASCII mockups; can't run commands |

### What agents can and can't do

- Writing files and running commands shows you the diff or command first and waits for `y`, `n`, or
  `a` (allow that tool for this chat tab).
- File tools stay inside the project, after resolving symlinks.
- Reading or writing `.env*`, key files, `.git/` and gitignored files always asks, even after `a`.
  `search` skips them.
- Commands run with the API key variables removed, a 120-second timeout and capped output.
- After 50 model turns on one message, the agent pauses and waits for you, to limit cost.

An agent's tool list only limits which tools it's offered. Review and Commit have no file-editing
tools, but a command you approve can still change files. Instructions such as Commit's "never
force-push" are guidance to the model, and nothing enforces them.

**There is no sandbox, so approval is the real security boundary.** An approved command can do
anything your user account can. Files and command output can contain instructions aimed at the
model (prompt injection), so read commands before you allow them, and be careful with `a` on
`run_command` in code you don't trust.

Chats aren't saved: they're gone when you quit.

## Tests

```sh
cargo test              # unit and integration tests, offline
tests/e2e.sh            # drives the real binary in an isolated tmux, against tests/mock_llm.py
```

Live tests are skipped by default. Some need no key; the rest make real, billed API calls:

```sh
cargo test -- --ignored live_openrouter_model_list live_claude_rejects   # no keys needed
ANTHROPIC_API_KEY=... cargo test -- --ignored live_claude_tool
OPENAI_API_KEY=... KALIPSO_TEST_MODEL=<model> cargo test -- --ignored live_openai_tool
XAI_API_KEY=... KALIPSO_TEST_MODEL=<model> cargo test -- --ignored live_grok_tool
OPENROUTER_API_KEY=... KALIPSO_TEST_MODEL=<model> cargo test -- --ignored live_openrouter_tool
```

## Not built yet

LSP and go-to-definition, git gutter, editor splits, multi-cursor, project-wide find UI, saved
chats, custom agents, MCP, a settings UI, in-app API key entry, mouse forwarding to programs in the
terminal pane, full Markdown rendering, and a Vim mode.

## License

MIT
