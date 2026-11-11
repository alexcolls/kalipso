# Installation

Every installer can be run again to update.

## Kalipso (desktop)

**Linux x86_64 and Apple silicon Macs:**

```sh
curl -fsSL https://raw.githubusercontent.com/alexcolls/kalipso/main/install.sh | sh
```

**Windows x86_64, in PowerShell:**

```powershell
irm https://raw.githubusercontent.com/alexcolls/kalipso/main/install.ps1 | iex
```

Kalipso then appears with your applications, and `kalipso [directory]` opens a window from a shell.
On Linux and macOS, `install.sh` also installs [kalipso-cli](#kalipso-cli-terminal-ide), which the
window opens. See [Kalipso desktop](desktop.md) for details and uninstalling.

## kalipso-cli (terminal IDE)

To install it on its own, for any terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/alexcolls/kalipso/main/install-cli.sh | sh
```

This installs the latest release to `~/.local/bin` and adds that folder to your `PATH` if it isn't
there yet.

| Platform | Result |
|---|---|
| Linux x86_64, glibc 2.39 or newer (Ubuntu 24.04, Debian 13 and later) | Prebuilt binary |
| Apple silicon macOS | Prebuilt binary |
| Anything else | Built from source: a few minutes, needs a C compiler. If Rust is missing or older than 1.89, a temporary copy is fetched for the build |
| Windows | Not native. Run the command in a WSL terminal, or in Git Bash, which hands over to WSL. `kalipso-cli` then works in WSL terminals, not in PowerShell or cmd |

### Manual install

Download a build from [Releases](https://github.com/alexcolls/kalipso/releases), unpack it, and put
`kalipso-cli` on your `PATH`.

### From source

```sh
git clone https://github.com/alexcolls/kalipso && cd kalipso
cargo run --release -- path/to/project
```

Requires Rust 1.89 or newer.

## Uninstalling

| Program | Delete |
|---|---|
| Kalipso, Linux | `~/.local/lib/kalipso`, `~/.local/bin/kalipso`, `~/.local/share/applications/kalipso.desktop` |
| Kalipso, macOS | `~/Applications/Kalipso.app`, `~/.local/bin/kalipso` |
| Kalipso, Windows | `%LOCALAPPDATA%\Programs\Kalipso`, its Start menu entry and its `PATH` entry |
| kalipso-cli | `~/.local/bin/kalipso-cli` and the `PATH` line the installer added to your shell config |

kalipso-cli settings and saved chats live in `~/.config/kalipso` and `~/.local/share/kalipso`.
