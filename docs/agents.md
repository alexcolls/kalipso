# AI providers and agents

## Claude Code, Codex and Antigravity

Starting a chat also offers three other vendors' agent CLIs, for using a subscription you already
have. Picking one opens it, as it ships, in a chat tab in the project folder. You sign in through its
own login, and kalipso-cli never sees or stores those credentials.

| Choice | Program | Sign in with |
|---|---|---|
| Claude Code | `claude` | A Claude Pro, Max, Ultra, Team or Enterprise plan, or a Claude Console account |
| Codex | `codex` | A ChatGPT plan, or an OpenAI API key |
| Antigravity | `agy` | A Google account |

kalipso-cli looks for the program on `PATH`, then in `~/.local/bin`, then on your interactive shell's
`PATH`, which covers installs through nvm or pnpm. If it isn't there, the tab shows the vendor's
official install command and runs it if you type `y` and press Enter; any other answer closes the
tab.

The tab is a terminal running that CLI. Keys go to it, Ctrl+P and Alt+F included, except kalipso-cli's
Alt keys for tabs and panes, Alt+S for scroll mode, and Ctrl+Q. The tab closes when the CLI exits.
These tabs aren't saved with your chats: each CLI keeps its own history.

## Providers

Keys come from environment variables only. kalipso-cli never writes them to disk or shows them.

| Provider | Key | Base URL override |
|---|---|---|
| Claude | `ANTHROPIC_API_KEY` | `ANTHROPIC_BASE_URL` |
| OpenAI | `OPENAI_API_KEY` | `OPENAI_BASE_URL` |
| Grok | `XAI_API_KEY` | `XAI_BASE_URL` |
| OpenRouter | `OPENROUTER_API_KEY` | `OPENROUTER_BASE_URL` |

Claude needs an API key from the Claude Console; a Claude subscription doesn't provide one. To use a
Claude plan, pick Claude Code instead.

Your model choice per provider is saved in `~/.config/kalipso/config.json` (`$XDG_CONFIG_HOME`
replaces `~/.config` if set). Claude defaults to `claude-opus-5-5`; for the others you pick from the
provider's list once.

For Claude models that support it, server-side fallbacks are on: if safety classifiers decline a
request, it's retried on a fallback model, billed at that model's rates.

A chat's agent and provider are fixed when it starts, because conversation history is stored in each
provider's own format. `/model` switches within a provider; otherwise open a new chat.

## Agents

| Agent | Tools | Job |
|---|---|---|
| Code | all | Implement changes |
| Test | all | Write and run tests, fix failures |
| Review | read + `run_command` | Review diffs and code; no file-editing tools |
| Commit | read + `run_command` | Inspect the diff, write the message, commit |
| Design | read + `write_file` | Design docs and ASCII mockups; can't run commands |

Each message also tells the agent which file is open, the cursor line, whether there are unsaved
changes, and any selected text (up to 32 KB). The transcript shows this under your message, for
example `with src/main.rs:42, 6 selected lines`. Selections in private files aren't sent.

## Long chats

Every turn resends the conversation, so at 150,000 input tokens (or 80% of the model's limit, where
the provider publishes it) kalipso-cli asks the model to summarize and continues from the summary. It
also does this once if a provider says the conversation is too long, and `/compact` does it on
request. The summary is built from the transcript without tool output, so the agent may need to read
files again. The transcript on screen stays as it was.

## Safety model

- Writing files and running commands shows you the diff or command first and waits for `y`, `n`, or
  `a` (allow that tool for this chat tab). For a file change, `v` opens the proposed file in a
  read-only editor tab with its changes marked, and you can answer from there.
- File tools stay inside the project, after resolving symlinks.
- Reading or writing `.env*`, key files, `.git/` and gitignored files always asks, even after `a`.
  `search` skips them.
- Commands run with the API key variables removed, a 120-second timeout and capped output.
- After 50 model turns on one message, the agent pauses and waits for you, to limit cost.

An agent's tool list only limits which tools it's offered. Review and Commit have no file-editing
tools, but a command you approve can still change files. Instructions such as Commit's "never
force-push" are guidance to the model, and nothing enforces them.

> **There is no sandbox, so approval is the real security boundary.** An approved command can do
> anything your user account can. Files and command output can contain instructions aimed at the model
> (prompt injection), so read commands before you allow them, and be careful with `a` on
> `run_command` in code you don't trust.

## Saved chats

Chats are saved once each finishes a turn, and the Alt+T picker offers them under `Resume`. They live
in `~/.local/share/kalipso/chats/` (`$XDG_DATA_HOME` replaces `~/.local/share` if set), one folder per
project, readable only by you, and the newest 50 per project are kept.

They hold the full conversation, including any code the agent read, so delete them if that matters.
An `a` answer isn't saved, so a resumed chat asks again.
