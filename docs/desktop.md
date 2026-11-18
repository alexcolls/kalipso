# Kalipso desktop

Kalipso is [WezTerm](https://wezterm.org) packaged with
[this configuration](https://github.com/alexcolls/wezterm-config), ten live-switchable themes and
FiraCode Nerd Font. It opens [kalipso-cli](cli.md) in the folder you start it from, or your login
shell if kalipso-cli isn't installed. On Windows it opens PowerShell.

It keeps its own copy of WezTerm and its config, so a WezTerm you already use is left alone.

## Launching

- From your applications menu or Start menu.
- From a shell: `kalipso [directory]`.

## Keys and themes

They are documented in the
[wezterm-config README](https://github.com/alexcolls/wezterm-config#keybindings). Its shell, prompt and
git settings aren't part of Kalipso.

Kalipso already passes the Alt keys through to [kalipso-cli](cli.md) while it runs.

## What's been tried

- **Linux:** opening the window on Wayland, and the Alt keys on X11.
- **macOS:** not run by hand. The Dock may show the name WezTerm.
- **Windows:** not run by hand. `kalipso-cli` only runs through WSL there, and its Alt keys may not get
  through from it. Kalipso needs an OpenGL driver, which some virtual machines lack: without one no
  window opens, and the log in `%USERPROFILE%\.local\share\wezterm` says why.

To uninstall, see [Installation](installation.md#uninstalling).
