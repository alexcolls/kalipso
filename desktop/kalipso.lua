-- What the Kalipso program changes in wezterm-config's wezterm.lua, which is base.lua here.
local wezterm = require 'wezterm'
-- Not require: its search path ignores --config-file, so it wouldn't look next to this file.
local config = dofile(wezterm.config_dir .. '/base.lua')

config.font_dirs = { 'fonts' } -- Relative to this file.
config.check_for_updates = false -- It would point at WezTerm's releases, not Kalipso's.
config.default_prog = nil -- The login shell. /bin/bash isn't everywhere.

if wezterm.target_triple:find 'windows' then
  config.default_prog = { 'powershell.exe', '-NoLogo' }
end
if not wezterm.target_triple:find 'linux' then
  config.window_decorations = 'RESIZE' -- Otherwise the window can't be resized.
end

return config
