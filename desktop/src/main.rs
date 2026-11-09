//! The Kalipso program: opens the bundled WezTerm with the bundled config, running kalipso-cli in it.
#![windows_subsystem = "windows"] // No console window when started from the Start menu.

use std::env;
use std::io;
use std::path::PathBuf;
use std::process::{Command, exit};

// Relative to this executable, as desktop/bundle.sh lays them out.
const GUI: &str = if cfg!(target_os = "macos") {
    "../Resources/WezTerm.app/Contents/MacOS/wezterm-gui"
} else if cfg!(windows) {
    "wezterm/wezterm-gui.exe"
} else {
    "wezterm/usr/bin/wezterm-gui"
};
const CONFIG: &str = if cfg!(target_os = "macos") {
    "../Resources/config/kalipso.lua"
} else {
    "config/kalipso.lua"
};

// kalipso-cli's installer puts it in ~/.local/bin, which a menu launch may not have on PATH.
#[cfg(unix)]
const HOST: &str = r#"PATH="$HOME/.local/bin:$PATH"
if command -v kalipso-cli >/dev/null; then exec kalipso-cli; fi
echo "kalipso-cli isn't installed. Install it with install-cli.sh from https://github.com/alexcolls/kalipso"
exec "${SHELL:-/bin/sh}" -l"#;

#[cfg(windows)]
unsafe extern "system" {
    fn AttachConsole(process_id: u32) -> i32;
}

fn main() -> io::Result<()> {
    // windows_subsystem also cuts it off from the console it was started in, so errors wouldn't show.
    #[cfg(windows)]
    unsafe {
        AttachConsole(u32::MAX); // ATTACH_PARENT_PROCESS. Fails harmlessly if there's no console.
    }

    let exe = env::current_exe()?;
    // Follows the ~/.local/bin symlink. Windows has none, and its canonical form is a \\?\ path.
    let exe = if cfg!(windows) {
        exe
    } else {
        exe.canonicalize()?
    };
    let dir = exe.parent().expect("an executable is in a folder");

    let mut command = Command::new(dir.join(GUI));
    command
        .arg("--config-file")
        .arg(dir.join(CONFIG))
        .args(["start", "--class", "kalipso"]);
    // WezTerm would open in the home folder instead, without saying so.
    let directory = match env::args_os().nth(1) {
        Some(directory) => Some(PathBuf::from(directory)),
        None => env::current_dir().ok(),
    };
    if let Some(directory) = directory {
        if !directory.is_dir() {
            eprintln!(
                "kalipso: {} isn't a directory\nusage: kalipso [directory]",
                directory.display()
            );
            exit(2);
        }
        command.arg("--cwd").arg(directory);
    }
    // The program is a host for kalipso-cli. Windows has none, so it gets a shell.
    #[cfg(unix)]
    command.args(["--", "sh", "-c", HOST]);
    run(command)
}

#[cfg(unix)]
fn run(mut command: Command) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    Err(command.exec())
}

#[cfg(windows)]
fn run(mut command: Command) -> io::Result<()> {
    command.spawn().map(drop)
}
