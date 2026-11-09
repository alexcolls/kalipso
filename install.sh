#!/bin/sh
# Installs the latest Kalipso program and puts the kalipso command on PATH. On Windows, use install.ps1.
# The terminal IDE is separate: install-cli.sh installs kalipso-cli.
#
#   curl -fsSL https://raw.githubusercontent.com/alexcolls/kalipso/main/install.sh | sh
set -eu

repo=alexcolls/kalipso

die() {
    echo "kalipso install: $*" >&2
    exit 1
}

add_to_path() {
    line='export PATH="$HOME/.local/bin:$PATH"'
    case ${SHELL:-} in
    */zsh) rc=${ZDOTDIR:-$HOME}/.zshrc ;;
    */bash) if [ "$os" = Darwin ]; then rc=$HOME/.bash_profile; else rc=$HOME/.bashrc; fi ;;
    */fish)
        rc=${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish
        line='fish_add_path $HOME/.local/bin'
        ;;
    *) rc=$HOME/.profile ;;
    esac
    mkdir -p "$(dirname "$rc")"
    grep -qsF "$line" "$rc" || printf '\n# Added by the Kalipso installer\n%s\n' "$line" >>"$rc"
    echo "Added ~/.local/bin to PATH in $rc. Open a new terminal, or run: $line"
}

install_linux() { # unpacked release folder
    app=$HOME/.local/lib/kalipso
    mkdir -p "$(dirname "$app")"
    rm -rf "$app"
    mv "$1" "$app"
    launcher=$app/kalipso

    apps=${XDG_DATA_HOME:-$HOME/.local/share}/applications
    mkdir -p "$apps"
    cat >"$apps/kalipso.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Kalipso
Comment=Futuristic open-source terminal
Exec="$launcher"
Icon=$app/wezterm/usr/share/icons/hicolor/128x128/apps/org.wezfurlong.wezterm.png
Terminal=false
Categories=System;TerminalEmulator;
StartupWMClass=kalipso
EOF
}

install_macos() { # unpacked release folder
    app=$HOME/Applications/Kalipso.app
    mkdir -p "$(dirname "$app")"
    rm -rf "$app"
    mv "$1/Kalipso.app" "$app"
    launcher=$app/Contents/MacOS/kalipso
}

# Everything runs from here, so a download cut short by `curl | sh` runs nothing.
main() {
    os=$(uname -s)
    arch=$(uname -m)
    case $os-$arch in
    Linux-x86_64)
        platform=x86_64-linux
        wezterm=wezterm/usr/bin/wezterm
        sha=sha256sum
        ;;
    Darwin-arm64)
        platform=aarch64-macos
        wezterm=Kalipso.app/Contents/Resources/WezTerm.app/Contents/MacOS/wezterm
        sha="shasum -a 256"
        ;;
    MINGW* | MSYS* | CYGWIN*)
        die "on Windows, run this in PowerShell: irm https://raw.githubusercontent.com/$repo/main/install.ps1 | iex"
        ;;
    *)
        die "there's no Kalipso build for $os $arch yet. The terminal IDE can still build here: see install-cli.sh in $repo."
        ;;
    esac
    command -v curl >/dev/null || die "curl is required."

    tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")
    tag=${tag##*/}
    case $tag in
    v*) ;;
    *) die "couldn't find the latest release of $repo." ;;
    esac

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    name=kalipso-$tag-$platform
    url=https://github.com/$repo/releases/download/$tag/$name.tar.gz
    # Each OS's build is uploaded a few minutes after the release appears.
    curl -fsL "$url" -o "$tmp/$name.tar.gz" ||
        die "couldn't download $name.tar.gz. If $tag came out in the last few minutes, try again soon."
    curl -fsSL "$url.sha256" -o "$tmp/$name.tar.gz.sha256"
    (cd "$tmp" && $sha -c "$name.tar.gz.sha256" >/dev/null) || die "checksum mismatch for $name.tar.gz"
    tar -xzf "$tmp/$name.tar.gz" -C "$tmp"

    # Checked before anything is replaced, so a system that can't run it keeps what it has.
    [ -x "$tmp/$name/$wezterm" ] || die "$name.tar.gz has no WezTerm in it, so $tag is an old release. Try again once a newer one is out."
    "$tmp/$name/$wezterm" --version >/dev/null 2>&1 || die "Kalipso's WezTerm doesn't run on this system."

    bin_dir=$HOME/.local/bin
    link=$bin_dir/kalipso
    if [ -f "$link" ] && [ ! -L "$link" ]; then
        echo "Replacing $link, the old terminal IDE. It's now kalipso-cli, which install-cli.sh installs."
    fi

    case $os in
    Linux) install_linux "$tmp/$name" ;;
    Darwin) install_macos "$tmp/$name" ;;
    esac
    mkdir -p "$bin_dir"
    ln -sf "$launcher" "$link"
    echo "Installed Kalipso $tag to $app"

    case :$PATH: in
    *":$bin_dir:"*) ;;
    *) add_to_path ;;
    esac
    echo "Open it from your applications, or run: kalipso [directory]"
}

main
