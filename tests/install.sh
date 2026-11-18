#!/bin/sh
# Drives install-cli.sh and install.sh offline, with stubs for everything that downloads, builds or asks the OS.
set -u

repo=$(cd "$(dirname "$0")/.." && pwd)
installer=$repo/install-cli.sh
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
stubs=$root/stubs
fixtures=$root/fixtures
mkdir "$stubs" "$fixtures" "$root/cargo-new" "$root/cargo-old" "$root/wsl"

stub() { # path, body
    printf '#!/bin/sh\n%s\n' "$2" >"$1"
    chmod +x "$1"
}

stub "$stubs/uname" 'case $1 in -s) echo "$FAKE_OS" ;; -m) echo "$FAKE_ARCH" ;; esac'
stub "$stubs/cc" ':'
stub "$stubs/sha256sum" '[ "$FAKE_OS" != Darwin ] || exit 127 # Macs have shasum instead.
exec /usr/bin/sha256sum "$@"'
stub "$stubs/xcode-select" '[ -z "${NO_XCODE:-}" ]'
stub "$root/wsl/wsl.exe" 'echo "$*" >"$FIXTURES/wsl-args"'
stub "$stubs/curl" 'while [ $# -gt 0 ]; do
    case $1 in -o) out=$2; shift ;; -w) shift ;; -*) ;; *) url=$1 ;; esac
    shift
done
case $url in
*/releases/latest) echo https://github.com/alexcolls/kalipso/releases/tag/v9.9.9 ;;
*sh.rustup.rs) cat "$FIXTURES/rustup.sh" ;;
*) cp "$FIXTURES/${url##*/}" "$out" ;;
esac'
for pair in new:1.99 old:1.75; do
    stub "$root/cargo-${pair%:*}/cargo" "[ \"\$1\" = --version ] && { echo 'cargo ${pair#*:}.0 (abc 2026-01-01)'; exit; }
# The workspace has two packages with binaries, so cargo install refuses to guess.
case \" \$* \" in *' kalipso-cli '*) ;; *) echo 'error: multiple packages with binaries found' >&2; exit 101 ;; esac
while [ \"\$1\" != --root ]; do shift; done
mkdir -p \"\$2/bin\"
printf '#!/bin/sh\n# built by %s\n' \"\$0\" >\"\$2/bin/kalipso-cli\"
chmod +x \"\$2/bin/kalipso-cli\""
done
stub "$fixtures/rustup.sh" 'mkdir -p "$CARGO_HOME/bin"
cp "$FIXTURES/cargo" "$CARGO_HOME/bin/cargo"'
cp "$root/cargo-new/cargo" "$fixtures/cargo"

usage='echo "usage: kalipso-cli [directory]" >&2; exit 2'
make_release() { # body of the packaged binary, platform
    release=kalipso-cli-v9.9.9-${2:-x86_64-linux}
    rm -rf "${fixtures:?}/$release"
    mkdir "$fixtures/$release"
    stub "$fixtures/$release/kalipso-cli" "$1"
    (cd "$fixtures" && tar czf "$release.tar.gz" "$release" && sha256sum "$release.tar.gz" >"$release.tar.gz.sha256")
}

failed=0
check() { # description, condition
    if eval "$2"; then
        echo "ok    $1"
    else
        echo "FAIL  $1"
        sed 's/^/      /' "$root/out"
        failed=1
    fi
}

n=0
new_home() {
    n=$((n + 1))
    home=$root/home$n
    mkdir "$home"
}

# Runs the installer in $home. Set os, arch, shell, extra_path or no_xcode for one call.
run() {
    env -i HOME="$home" FIXTURES="$fixtures" PATH="${extra_path:-}$stubs:/usr/bin:/bin" \
        FAKE_OS="${os:-Linux}" FAKE_ARCH="${arch:-x86_64}" SHELL="${shell:-/usr/bin/zsh}" NO_XCODE="${no_xcode:-}" \
        sh "$installer" >"$root/out" 2>&1
    status=$?
    extra_path= os= arch= shell= no_xcode= # Whether these outlive the call depends on the shell.
    return "$status"
}

make_release "$usage"
new_home
run
check "downloads the release" '[ -x "$home/.local/bin/kalipso-cli" ] && ! grep -q built "$home/.local/bin/kalipso-cli"'
check "adds the folder to PATH" 'grep -q "\.local/bin" "$home/.zshrc"'
run
check "doesn't add it twice" '[ "$(grep -c "\.local/bin" "$home/.zshrc")" = 1 ]'

new_home
extra_path=$home/.local/bin: run
check "leaves shell files alone when already on PATH" '[ ! -e "$home/.zshrc" ]'

new_home
shell=/usr/bin/fish run
check "uses fish's own syntax" 'grep -q fish_add_path "$home/.config/fish/config.fish"'
new_home
shell=/bin/ksh run
check "falls back to .profile for other shells" 'grep -q "\.local/bin" "$home/.profile"'

new_home
echo "0000  $release.tar.gz" >"$fixtures/$release.tar.gz.sha256"
check "refuses a bad checksum" '! run && [ ! -e "$home/.local/bin/kalipso-cli" ] && grep -q "checksum mismatch" "$root/out"'

make_release 'echo "version GLIBC_2.99 not found" >&2; exit 1'
new_home
extra_path=$root/cargo-new: run
check "builds from source when the release can't run" 'grep -q "built by $root/cargo-new" "$home/.local/bin/kalipso-cli"'
make_release "echo \"weak version GLIBC_2.39 not found\" >&2; $usage" # Seen for real on Ubuntu 22.04.
new_home
extra_path=$root/cargo-new: run
check "builds from source when the release warns on every launch" 'grep -q "built by $root/cargo-new" "$home/.local/bin/kalipso-cli"'
make_release "$usage"

new_home
extra_path=$root/cargo-new: os=Darwin arch=arm64 shell=/bin/bash run
check "builds on Apple silicon when the release has no Mac build" 'grep -q "built by $root/cargo-new" "$home/.local/bin/kalipso-cli"'
check "uses .bash_profile on macOS" 'grep -q "\.local/bin" "$home/.bash_profile"'
make_release "$usage" aarch64-macos
new_home
os=Darwin arch=arm64 run
check "downloads the Mac build on Apple silicon" '[ -x "$home/.local/bin/kalipso-cli" ] && ! grep -q built "$home/.local/bin/kalipso-cli"'
new_home
extra_path=$root/cargo-new: os=Darwin arch=x86_64 run
check "builds on Intel Macs" 'grep -q "built by $root/cargo-new" "$home/.local/bin/kalipso-cli"'
new_home
check "asks for Apple's tools on macOS" '! os=Darwin no_xcode=1 run && grep -q "xcode-select --install" "$root/out"'

new_home
extra_path=$root/cargo-old: arch=aarch64 run
check "replaces a too old Rust" 'grep -q "built by .*/cargo/bin/cargo" "$home/.local/bin/kalipso-cli"'
new_home
arch=aarch64 run
check "fetches Rust when there is none" 'grep -q "built by .*/cargo/bin/cargo" "$home/.local/bin/kalipso-cli"'

new_home
extra_path=$root/wsl: os=MINGW64_NT-10.0 run
check "hands over to WSL on Windows" 'grep -q "exec sh -c curl .*install-cli.sh | sh" "$fixtures/wsl-args" && [ ! -e "$home/.local" ]'
new_home
check "explains WSL when it's missing" '! os=MINGW64_NT-10.0 run && grep -q "wsl --install" "$root/out"'

# The Kalipso program, from install.sh.
installer=$repo/install.sh
app=.local/lib/kalipso
make_program() { # body of the bundled wezterm, platform
    platform=${2:-x86_64-linux}
    program=kalipso-v9.9.9-$platform
    rm -rf "${fixtures:?}/$program"
    case $platform in
    *linux) wezterm=$fixtures/$program/wezterm/usr/bin/wezterm-gui launcher=$fixtures/$program/kalipso ;;
    *macos)
        wezterm=$fixtures/$program/Kalipso.app/Contents/Resources/WezTerm.app/Contents/MacOS/wezterm
        launcher=$fixtures/$program/Kalipso.app/Contents/MacOS/kalipso
        ;;
    esac
    mkdir -p "$(dirname "$wezterm")" "$(dirname "$launcher")"
    stub "$wezterm" "$1"
    stub "$launcher" ':'
    (cd "$fixtures" && tar czf "$program.tar.gz" "$program" && sha256sum "$program.tar.gz" >"$program.tar.gz.sha256")
}

# install.sh downloads install-cli.sh, which then finds the kalipso-cli releases made above.
cp "$repo/install-cli.sh" "$fixtures/install-cli.sh"
make_program ':'
new_home
run
check "installs the program" '[ -x "$home/$app/kalipso" ]'
check "links the kalipso command to it" '[ "$(readlink "$home/.local/bin/kalipso")" = "$home/$app/kalipso" ]'
check "adds a desktop entry" 'grep -qx "Exec=\"$home/$app/kalipso\"" "$home/.local/share/applications/kalipso.desktop" && grep -qx StartupWMClass=kalipso "$home/.local/share/applications/kalipso.desktop"'
check "installs kalipso-cli, which the window runs" '[ -x "$home/.local/bin/kalipso-cli" ] && ! grep -q built "$home/.local/bin/kalipso-cli"'
check "adds the command's folder to PATH" 'grep -q "\.local/bin" "$home/.zshrc"'
check "says it added the folder to PATH once" '[ "$(grep -c "Added ~/.local/bin" "$root/out")" = 1 ]'
touch "$home/$app/stale"
run
check "replaces an older install" '[ -x "$home/$app/kalipso" ] && [ ! -e "$home/$app/stale" ]'
check "doesn't add it to PATH twice" '[ "$(grep -c "\.local/bin" "$home/.zshrc")" = 1 ]'

new_home
mkdir -p "$home/.local/bin"
echo old >"$home/.local/bin/kalipso"
run
check "replaces the old terminal IDE and says where it went" '[ -L "$home/.local/bin/kalipso" ] && grep -q "now kalipso-cli" "$root/out" && [ -x "$home/.local/bin/kalipso-cli" ]'

stub "$fixtures/install-cli.sh" 'echo "kalipso-cli install: no C compiler" >&2; exit 1'
new_home
check "says when kalipso-cli didn't install, after installing the program" '! run && [ -x "$home/$app/kalipso" ] && grep -q "no C compiler" "$root/out" && grep -q "kalipso-cli isn.t" "$root/out" && grep -q "\.local/bin" "$home/.zshrc"'
rm "$fixtures/install-cli.sh"
new_home
check "says when install-cli.sh can't be downloaded" '! run && [ -x "$home/$app/kalipso" ] && grep -q "kalipso-cli isn.t" "$root/out"'
cp "$repo/install-cli.sh" "$fixtures/install-cli.sh"

new_home
run
touch "$home/$app/kept"
echo "0000  $program.tar.gz" >"$fixtures/$program.tar.gz.sha256"
check "refuses a bad checksum and keeps what's installed" '! run && [ -e "$home/$app/kept" ] && grep -q "checksum mismatch" "$root/out"'
make_program 'echo "wezterm-gui: error while loading shared libraries: libxkbcommon-x11.so.0" >&2; exit 127'
check "refuses a build that can't run here and keeps what's installed" '! run && [ -e "$home/$app/kept" ] && grep -q "run on this system" "$root/out"'
check "says why it can't run" 'grep -q "libxkbcommon-x11.so.0" "$root/out"'
rm "$fixtures/$program.tar.gz"
check "explains a build that isn't uploaded yet" '! run && [ -e "$home/$app/kept" ] && grep -q "try again soon" "$root/out"'

make_program ':' aarch64-macos
new_home
os=Darwin arch=arm64 run
check "installs the app on Apple silicon" '[ -x "$home/Applications/Kalipso.app/Contents/MacOS/kalipso" ] && [ "$(readlink "$home/.local/bin/kalipso")" = "$home/Applications/Kalipso.app/Contents/MacOS/kalipso" ]'
check "adds no desktop entry on macOS" '[ ! -e "$home/.local/share/applications" ]'
check "installs kalipso-cli on Apple silicon" '[ -x "$home/.local/bin/kalipso-cli" ] && ! grep -q built "$home/.local/bin/kalipso-cli"'

new_home
check "has no build for Intel Macs" '! os=Darwin arch=x86_64 run && grep -q "no Kalipso build" "$root/out" && [ ! -e "$home/.local" ]'
new_home
check "has no build for Linux on ARM" '! arch=aarch64 run && grep -q "no Kalipso build" "$root/out" && [ ! -e "$home/.local" ]'
new_home
check "points Git Bash to install.ps1" '! os=MINGW64_NT-10.0 run && grep -q "install.ps1" "$root/out" && [ ! -e "$home/.local" ]'

exit "$failed"
