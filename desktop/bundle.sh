#!/usr/bin/env bash
# Builds the Kalipso program for this OS: the launcher, a pinned WezTerm, the config and a font.
# Fails unless the result loads its config and finds its font. Leaves the archive and its .sha256
# in target/bundle. On Linux it also needs xvfb-run, to open the window once.
#
#   desktop/bundle.sh <tag>
set -euo pipefail

tag=${1:?usage: desktop/bundle.sh <tag>}

wezterm=20240203-110809-5046fc22
wezterm_url=https://github.com/wezterm/wezterm/releases/download/$wezterm
license_sha=37db33bbbd7348969eda397b89a16f252d56c1ca7481b6ccaf56ccdcbab5dcca
config_commit=354aee79e58716746aad49413241ff6589771598
config_sha=e212dedce0aaab8ec70e68ded1ed1c567e329b7a341f68dac5158ecae33b17c0
font_release=v3.5.1
font_sha=68e3bd6164864b8b514605bc34e3a87ac401c8c48682fcce6478c70263340207

cd "$(dirname "$0")/.."
repo=$PWD
work=$repo/target/bundle
mkdir -p "$work/downloads"

if [ "$(uname -s)" = Darwin ]; then sha="shasum -a 256"; else sha=sha256sum; fi

fetch() { # url, sha256. Leaves the path in $file. Reuses an earlier download, as the sha256 pins it.
    file=$work/downloads/${1##*/}
    if [ ! -e "$file" ]; then
        curl -fsSL --retry 3 "$1" -o "$file.part"
        mv "$file.part" "$file"
    fi
    (cd "$work/downloads" && echo "$2  ${file##*/}" | $sha -c >/dev/null)
}

case $(uname -s) in
Linux) platform=x86_64-linux ;;
Darwin) platform=aarch64-macos ;;
MINGW* | MSYS*) platform=x86_64-windows ;;
*)
    echo "desktop/bundle.sh: no build for $(uname -s)" >&2
    exit 1
    ;;
esac
name=kalipso-$tag-$platform
out=$work/$name
rm -rf "$out"
mkdir "$out"

case $platform in
x86_64-linux)
    # Static, so glibc doesn't limit where this runs. The AppImage's own libraries are from Ubuntu 20.04.
    rustup target add x86_64-unknown-linux-musl
    cargo build --release --locked -p kalipso --target x86_64-unknown-linux-musl
    launcher=target/x86_64-unknown-linux-musl/release/kalipso
    cp "$launcher" "$out/kalipso"

    fetch "$wezterm_url/WezTerm-$wezterm-Ubuntu20.04.AppImage" 34010a07076d2272c4d4f94b5e0dae608a679599e8d729446323f88f956c60f0
    chmod +x "$file"
    # Extracted, so users don't need FUSE.
    (cd "$out" && "$file" --appimage-extract >/dev/null && mv squashfs-root wezterm)
    resources=$out
    cli=$out/wezterm/usr/bin/wezterm
    ;;
aarch64-macos)
    cargo build --release --locked -p kalipso
    contents=$out/Kalipso.app/Contents
    mkdir -p "$contents/MacOS"
    launcher=target/release/kalipso
    cp "$launcher" "$contents/MacOS/kalipso"

    fetch "$wezterm_url/WezTerm-macos-$wezterm.zip" e77388cad55f2e9da95a220a89206a6c58f865874a629b7c3ea3c162f5692224
    ditto -x -k "$file" "$out/unpacked"
    mkdir "$contents/Resources"
    mv "$out/unpacked/WezTerm-macos-$wezterm/WezTerm.app" "$contents/Resources/"
    rm -rf "$out/unpacked"
    # Upstream's signature and notarization only hold while its app is unmodified.
    codesign --verify --deep "$contents/Resources/WezTerm.app"
    cp "$contents/Resources/WezTerm.app/Contents/Resources/terminal.icns" "$contents/Resources/kalipso.icns"
    cat >"$contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Kalipso</string>
  <key>CFBundleIdentifier</key><string>com.alexcolls.kalipso</string>
  <key>CFBundleExecutable</key><string>kalipso</string>
  <key>CFBundleIconFile</key><string>kalipso</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${tag#v}</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
    resources=$contents/Resources
    cli=$resources/WezTerm.app/Contents/MacOS/wezterm
    ;;
x86_64-windows)
    cargo build --release --locked -p kalipso
    launcher=target/release/kalipso.exe
    cp "$launcher" "$out/kalipso.exe"

    fetch "$wezterm_url/WezTerm-windows-$wezterm.zip" 57e5d03b585303d81e8b8e96d1230362852eb39aca92b3b29c7a42cfb82f9ac4
    unzip -q "$file" -d "$out"
    mv "$out/WezTerm-windows-$wezterm" "$out/wezterm"
    resources=$out
    cli=$out/wezterm/wezterm.exe
    ;;
esac

config=$resources/config/kalipso.lua
mkdir -p "$resources/config/fonts" "$resources/licenses"
cp desktop/kalipso.lua "$config"
fetch "https://raw.githubusercontent.com/alexcolls/wezterm-config/$config_commit/config/wezterm/wezterm.lua" "$config_sha"
cp "$file" "$resources/config/base.lua"
fetch "https://github.com/ryanoasis/nerd-fonts/releases/download/$font_release/FiraCode.tar.xz" "$font_sha"
tar -xJf "$file" -C "$resources/config/fonts" \
    FiraCodeNerdFont-Light.ttf FiraCodeNerdFont-Regular.ttf FiraCodeNerdFont-Bold.ttf
tar -xJOf "$file" LICENSE >"$resources/licenses/FiraCode.txt"
fetch "https://raw.githubusercontent.com/wezterm/wezterm/$wezterm/LICENSE.md" "$license_sha"
cp "$file" "$resources/licenses/WezTerm.txt"
cp LICENSE "$resources/licenses/Kalipso.txt"

"$cli" --config-file "$config" show-keys >/dev/null
"$cli" --config-file "$config" ls-fonts --text a | grep -q FiraCodeNerdFont-Light

# From target/, with no WezTerm next to it, so getting this wrong can't leave a window up.
status=0
"$launcher" /nonexistent 2>/dev/null || status=$?
if [ "$status" != 2 ]; then
    echo "desktop/bundle.sh: a missing directory wasn't refused (status $status)" >&2
    exit 1
fi

cd "$work"
if [ "$platform" = x86_64-linux ]; then
    # Through a symlink, as installed, with a stand-in kalipso-cli where install-cli.sh puts it. It
    # says where it ran, then stays up. 124 is timeout ending it, so the window stayed up.
    rm -rf home ran-in
    mkdir -p home/.local/bin
    printf '#!/bin/sh\npwd -P >"%s"\nexec sleep 60\n' "$work/ran-in" >home/.local/bin/kalipso-cli
    chmod +x home/.local/bin/kalipso-cli
    ln -sf "$out/kalipso" kalipso-link
    status=0
    HOME=$work/home env -u WAYLAND_DISPLAY xvfb-run -a timeout 8 ./kalipso-link || status=$?
    rm kalipso-link
    if [ "$status" != 124 ]; then
        echo "desktop/bundle.sh: the window didn't stay open (status $status)" >&2
        exit 1
    fi
    # An empty window also stays open.
    if [ "$(cat ran-in 2>/dev/null)" != "$(pwd -P)" ]; then
        echo "desktop/bundle.sh: the window didn't run kalipso-cli in the current directory" >&2
        exit 1
    fi
    rm -rf home ran-in
fi

if [ "$platform" = x86_64-windows ]; then
    archive=$name.zip
    rm -f "$archive"
    7z a -tzip -bd "$archive" "$name" >/dev/null
else
    archive=$name.tar.gz
    COPYFILE_DISABLE=1 tar czf "$archive" "$name"
fi
$sha "$archive" >"$archive.sha256"
echo "$work/$archive"
