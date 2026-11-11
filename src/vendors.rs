//! Other vendors' coding agents, run unmodified in a chat tab. People sign in through the vendor's
//! own login, with their subscription or a key. Kalipso never sees or stores those credentials:
//! the tab is a terminal running the vendor's program, as if it had been typed in a shell.

use std::path::Path;

use portable_pty::CommandBuilder;

use crate::term::Term;

pub struct Vendor {
    pub name: &'static str,
    pub about: &'static str,
    /// The program the vendor's installer puts on PATH.
    program: &'static str,
    /// The vendor's own installer, as its docs give it.
    install: &'static str,
}

pub const VENDORS: [Vendor; 3] = [
    Vendor {
        name: "Claude Code",
        about: "Anthropic's agent CLI, sign in with your Claude plan or an API key",
        program: "claude",
        install: "curl -fsSL https://claude.ai/install.sh | bash",
    },
    Vendor {
        name: "Codex",
        about: "OpenAI's agent CLI, sign in with ChatGPT or an API key",
        program: "codex",
        install: "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
    },
    Vendor {
        name: "Antigravity",
        about: "Google's agent CLI, sign in with your Google account",
        program: "agy",
        install: "curl -fsSL https://antigravity.google/cli/install.sh | bash",
    },
];

/// Runs the vendor's program, or offers its installer first. $1 is the program, $2 its name and
/// $3 the install command.
const LAUNCH: &str = r#"program=$1 name=$2 install=$3
shell=${SHELL:-/bin/sh}
# Where the vendors' installers put their programs, even before the shell config says so.
PATH=$HOME/.local/bin:$PATH
export PATH

start() {
    if command -v "$program" >/dev/null 2>&1; then
        exec "$program"
    fi
    # Installed through nvm, pnpm and the like, it may only be on the interactive shell's PATH.
    if "$shell" -i -c "command -v $program" </dev/null >/dev/null 2>&1; then
        exec "$shell" -i -c "exec $program"
    fi
}

start
printf "%s isn't installed. Its official installer is:\n\n    %s\n\nPress Enter to run it here, or Alt+W to close this tab.\n" "$name" "$install"
read -r _ || exit 1
sh -c "$install" && start
printf "\n%s didn't install. Press Enter to close this tab.\n" "$name"
read -r _"#;

impl Vendor {
    /// The command for a tab in `root`. `env` is added to what the tab inherits.
    pub fn launch(&self, root: &Path, env: &[(String, String)]) -> CommandBuilder {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            LAUNCH,
            "kalipso",
            self.program,
            self.name,
            self.install,
        ]);
        command.cwd(root);
        for (key, value) in env {
            command.env(key, value);
        }
        command
    }

    pub fn label(&self) -> String {
        format!("{}: {}", self.name, self.about)
    }
}

/// A chat tab running a vendor's program.
pub struct VendorTab {
    pub vendor: &'static Vendor,
    pub term: Term,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Event;
    use crate::term::spawn;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kalipso-vendor-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn script(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A tab with a fixed PATH and HOME, so the real CLIs on this machine never start.
    fn run(vendor: &Vendor, home: &Path, shell: &str) -> (Term, mpsc::Receiver<Event>) {
        let env = vec![
            ("HOME".to_owned(), home.display().to_string()),
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("SHELL".to_owned(), shell.to_owned()),
        ];
        let (tx, rx) = mpsc::sync_channel(256);
        let term = spawn(vendor.launch(home, &env), 1, (24, 100), tx).unwrap();
        (term, rx)
    }

    fn wait_for(term: &mut Term, rx: &mpsc::Receiver<Event>, wanted: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Event::PtyOutput { bytes, .. }) = rx.recv_timeout(Duration::from_millis(100))
            {
                term.feed(&bytes);
            }
            if term.contents().contains(wanted) {
                return true;
            }
        }
        false
    }

    #[test]
    fn runs_the_installed_program_in_the_project() {
        let home = dir("installed");
        script(
            &home.join(".local/bin/claude"),
            "echo \"fake claude in $(pwd)\"; sleep 5",
        );
        let (mut term, rx) = run(&VENDORS[0], &home, "/bin/sh");
        let wanted = format!("fake claude in {}", home.display());
        assert!(wait_for(&mut term, &rx, &wanted), "{}", term.contents());
    }

    #[test]
    fn finds_programs_only_the_interactive_shell_knows() {
        let home = dir("shell-path");
        script(&home.join("nvm/codex"), "echo fake codex; sleep 5");
        // Stands in for a shell whose config adds nvm's folder to PATH.
        let shell = home.join("shell");
        script(
            &shell,
            &format!("PATH={}/nvm:$PATH exec /bin/sh \"$@\"", home.display()),
        );
        let (mut term, rx) = run(&VENDORS[1], &home, &shell.display().to_string());
        assert!(
            wait_for(&mut term, &rx, "fake codex"),
            "{}",
            term.contents()
        );
    }

    #[test]
    fn offers_the_official_installer_and_runs_it_on_enter() {
        let home = dir("install");
        let vendor = Vendor {
            name: "Fake",
            about: "",
            program: "fakecli",
            install: "mkdir -p \"$HOME/.local/bin\" && printf '#!/bin/sh\\necho now running; sleep 5\\n' >\"$HOME/.local/bin/fakecli\" && chmod +x \"$HOME/.local/bin/fakecli\"",
        };
        let (mut term, rx) = run(&vendor, &home, "/bin/sh");
        assert!(
            wait_for(&mut term, &rx, "Fake isn't installed"),
            "{}",
            term.contents()
        );
        assert!(
            term.contents().contains("mkdir -p"),
            "shows the install command"
        );
        let mut scroll = false;
        term.on_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut scroll,
        );
        assert!(
            wait_for(&mut term, &rx, "now running"),
            "{}",
            term.contents()
        );
    }

    #[test]
    fn says_when_the_installer_fails() {
        let home = dir("install-fails");
        let vendor = Vendor {
            name: "Fake",
            about: "",
            program: "fakecli",
            install: "false",
        };
        let (mut term, rx) = run(&vendor, &home, "/bin/sh");
        assert!(
            wait_for(&mut term, &rx, "isn't installed"),
            "{}",
            term.contents()
        );
        let mut scroll = false;
        term.on_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut scroll,
        );
        assert!(
            wait_for(&mut term, &rx, "Fake didn't install"),
            "{}",
            term.contents()
        );
    }

    #[test]
    fn installers_are_the_vendors_own() {
        for vendor in &VENDORS {
            assert!(
                vendor.install.starts_with("curl -fsSL https://")
                    && [
                        "https://claude.ai/",
                        "https://chatgpt.com/",
                        "https://antigravity.google/"
                    ]
                    .iter()
                    .any(|host| vendor.install.contains(host)),
                "{}",
                vendor.install
            );
        }
    }
}
