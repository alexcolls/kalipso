#!/bin/sh
# Installs the latest Kalipso to ~/.local/bin and puts that folder on PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/alexcolls/kalipso/main/install.sh | sh
set -eu

repo=alexcolls/kalipso
min_rust_minor=89 # What Cargo.lock's dependencies need, as in Rust 1.89.

die() {
    echo "kalipso install: $*" >&2
    exit 1
}

install_in_wsl() {
    # Kalipso uses std::os::unix, so it doesn't compile for Windows. WSL is Linux, so it runs there.
    command -v wsl.exe >/dev/null ||
        die "on Windows, Kalipso runs in WSL. Run 'wsl --install' in an admin PowerShell, reboot, then run this again."
    echo "Windows detected, installing inside WSL."
    MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' wsl.exe --exec sh -c \
        "curl -fsSL https://raw.githubusercontent.com/$repo/main/install.sh | sh"
    echo "Open a WSL terminal to use the kalipso command."
}

# Leaves the path of a verified binary that runs here in $bin, or $bin empty if there isn't one.
download_release() { # platform, as the release names it
    name=kalipso-$tag-$1
    url=https://github.com/$repo/releases/download/$tag/$name.tar.gz
    # Macs get theirs after Linux, and older releases have none, so this may not exist.
    curl -fsL "$url" -o "$tmp/$name.tar.gz" || return 0
    curl -fsSL "$url.sha256" -o "$tmp/$name.tar.gz.sha256"
    if [ "$os" = Darwin ]; then sha="shasum -a 256"; else sha=sha256sum; fi
    (cd "$tmp" && $sha -c "$name.tar.gz.sha256" >/dev/null) || die "checksum mismatch for $name.tar.gz"
    tar -xzf "$tmp/$name.tar.gz" -C "$tmp"

    # Its usage error first means it started cleanly. musl can't load it, and glibc older than
    # the build's refuses it, or starts it but prints a loader warning on every launch.
    output=$("$tmp/$name/kalipso" /nonexistent 2>&1) || :
    case $output in
    usage:*) bin=$tmp/$name/kalipso ;;
    esac
}

build_from_source() {
    echo "No prebuilt binary runs on this system, so building $tag from source. This takes a few minutes."
    if [ "$os" = Darwin ]; then
        xcode-select -p >/dev/null 2>&1 ||
            die "building needs Apple's command line tools. Run 'xcode-select --install', then run this again."
    fi
    command -v cc >/dev/null || die "building needs a C compiler. Install gcc or clang, then run this again."

    PATH=${CARGO_HOME:-$HOME/.cargo}/bin:$PATH
    minor=$(cargo --version 2>/dev/null | cut -d. -f2)
    if [ "${minor:-0}" -lt "$min_rust_minor" ]; then
        echo "Needs Rust 1.$min_rust_minor or newer, so fetching a temporary copy just for this build."
        export CARGO_HOME="$tmp/cargo" RUSTUP_HOME="$tmp/rustup"
        curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path
        PATH=$CARGO_HOME/bin:$PATH
    fi

    # The PATH entry only stops cargo warning that this temporary folder isn't on PATH.
    PATH=$tmp/build/bin:$PATH cargo install --locked --git "https://github.com/$repo" --tag "$tag" --root "$tmp/build"
    bin=$tmp/build/bin/kalipso
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

# Everything runs from here, so a download cut short by `curl | sh` runs nothing.
main() {
    os=$(uname -s)
    case $os in
    MINGW* | MSYS* | CYGWIN*)
        install_in_wsl
        return
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
    bin=
    case $os-$(uname -m) in
    Linux-x86_64) download_release x86_64-linux ;;
    Darwin-arm64) download_release aarch64-macos ;;
    esac
    if [ -z "$bin" ]; then
        build_from_source
    fi

    bin_dir=$HOME/.local/bin
    mkdir -p "$bin_dir"
    install -m 755 "$bin" "$bin_dir/kalipso"
    echo "Installed Kalipso $tag to $bin_dir/kalipso"

    case :$PATH: in
    *":$bin_dir:"*) ;;
    *) add_to_path ;;
    esac
    echo "Run it with: kalipso [directory]"
}

main
