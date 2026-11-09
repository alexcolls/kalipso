# Troubleshooting

## Alt keys don't reach kalipso-cli

Terminals can keep these shortcuts for themselves. Clicking a pane always works as a fallback.

### macOS

The Option key types special characters by default, so make it act as Alt.

- **Terminal:** Settings > Profiles > Keyboard > Use Option as Meta key.
- **iTerm2:** Settings > Profiles > Keys > General > Left Option key > Esc+.

### WezTerm

The [Kalipso desktop program](desktop.md) already passes these keys through. For your own WezTerm:
kalipso-cli sets a `KALIPSO` user variable while it runs, so a config can pass the keys through only
then. Wrap your existing bindings:

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

### tmux

If your config binds Alt+arrows:

```tmux
bind -n M-Left  if -F '#{==:#{pane_current_command},kalipso-cli}' 'send-keys M-Left'  'select-pane -L'
bind -n M-Right if -F '#{==:#{pane_current_command},kalipso-cli}' 'send-keys M-Right' 'select-pane -R'
bind -n M-Up    if -F '#{==:#{pane_current_command},kalipso-cli}' 'send-keys M-Up'    'select-pane -U'
bind -n M-Down  if -F '#{==:#{pane_current_command},kalipso-cli}' 'send-keys M-Down'  'select-pane -D'
```

## Kalipso opens no window on Windows

It needs an OpenGL driver, which some virtual machines lack. The log in
`%USERPROFILE%\.local\share\wezterm` says why.

## kalipso-cli doesn't run on Windows

It only runs in WSL terminals (or Git Bash, which hands over to WSL), not in PowerShell or cmd.

## The agent can't be chosen / no chat opens

No provider key is set. Export one from [AI providers](agents.md#providers) before starting
kalipso-cli.

## F12 or F8 does nothing

Install the language server for your language and answer the first-use prompt. See
[Language servers](language-servers.md).

## Selecting text copies only one pane

kalipso-cli captures the mouse. Hold Shift for your terminal's own selection.
