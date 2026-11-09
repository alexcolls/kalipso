# Language servers

kalipso-cli uses whichever of these is installed:

| Language | Server |
|---|---|
| Rust | `rust-analyzer` (`rustup component add rust-analyzer`) |
| Python | `pyright-langserver` or `pylsp` |
| TypeScript / JavaScript | `typescript-language-server` |
| Go | `gopls` |
| C / C++ | `clangd` |

## Consent

**Language servers run code from the project they open**, such as build scripts and macros, so opening
a file never starts one. The first F12 or F8 in a project asks you. A yes is saved for that project in
`~/.config/kalipso/config.json`, and the server then starts by itself there.

API keys are removed from the server's environment, and it stops when kalipso-cli does.

## Supported features

| Key | Feature |
|---|---|
| F12 | Go to definition (the first location if there are several) |
| Shift+F12 | Come back |
| F8 | Move between problems, which are also shown next to their lines |

Completion, hover and rename are not built yet.
