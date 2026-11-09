# Development

## Layout

| Path | Contents |
|---|---|
| `src/` | kalipso-cli: `app` (UI and key handling), `chat`, `llm` (providers), `tools` (agent tools), `editor`, `files`, `git`, `lsp`, `term` (PTY and VT emulation), `main` |
| `desktop/` | The Kalipso program: bundle script, WezTerm config (`kalipso.lua`), launcher |
| `tests/` | End-to-end and installer tests, plus mock LLM and language servers |
| `install.sh`, `install.ps1`, `install-cli.sh` | Installers |
| `docs/` | This documentation and its [web viewer](index.html) |

## Build and run

```sh
cargo run --release -- path/to/project
```

Rust 1.89 or newer.

## Tests

```sh
cargo test              # unit and integration tests, offline
tests/e2e.sh            # drives the real binary in an isolated tmux, against tests/mock_llm.py and tests/mock_lsp.py
tests/install.sh        # drives install-cli.sh and install.sh offline, with stubs for downloads and builds
desktop/bundle.sh dev   # builds the Kalipso program for this OS into target/bundle, and checks it loads
tests/install.ps1       # Windows only: drives install.ps1 against that bundle, then opens the program
```

All of these run in CI, with `cargo fmt --check` and `cargo clippy`.

### Live tests

Skipped by default. Some need no key; the rest make real, billed API calls:

```sh
cargo test -- --ignored live_openrouter_model_list live_claude_rejects   # no keys needed
ANTHROPIC_API_KEY=... cargo test -- --ignored live_claude_tool
OPENAI_API_KEY=... KALIPSO_TEST_MODEL=<model> cargo test -- --ignored live_openai_tool
XAI_API_KEY=... KALIPSO_TEST_MODEL=<model> cargo test -- --ignored live_grok_tool
OPENROUTER_API_KEY=... KALIPSO_TEST_MODEL=<model> cargo test -- --ignored live_openrouter_tool
```

## Releasing

Pushing a tag like `v0.1.0` publishes both programs as a GitHub release: kalipso-cli for Linux x86_64
and Apple silicon, and Kalipso for those and Windows.

## Viewing these docs locally

The docs web is a static page that fetches the Markdown files, so serve the folder:

```sh
python3 -m http.server -d docs 8000   # then open http://localhost:8000
```
