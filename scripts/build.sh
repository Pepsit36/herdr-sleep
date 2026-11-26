#!/bin/sh
# Build hook of the Herdr Sleep plugin (ADR 0006).
#
# Installs bin/herdr-sleep and bin/herdr-sleep-ph: from the GitHub release that
# matches this checkout when there is one, otherwise from source with cargo.
#
# Environment (mostly for tests):
#   HERDR_SLEEP_RELEASE_BASE_URL  release download base URL (file:// accepted)
#   HERDR_SLEEP_FORCE_CARGO=1     skip the download and build from source
set -eu

REPO_DEFAULT_BASE="https://github.com/Pepsit36/herdr-sleep/releases/download"
BINARIES="herdr-sleep herdr-sleep-ph"

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

say() { printf 'herdr-sleep build: %s\n' "$*"; }

tmp=""
cleanup() { [ -n "$tmp" ] && rm -rf "$tmp"; return 0; }
# A signal ends the script (the EXIT trap then cleans up) instead of letting it
# carry on with the source build.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Install $1 as bin/$2 without ever rewriting a possibly running executable.
install_binary() {
    mkdir -p bin
    cp "$1" "bin/$2.new"
    chmod +x "bin/$2.new"
    mv -f "bin/$2.new" "bin/$2"
}

manifest_version() {
    sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' herdr-plugin.toml | head -n 1
}

fetch() { # url -> stdout
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "$1"
    else
        echo "neither curl nor wget is installed" >&2
        return 1
    fi
}

sha256_of() { # file -> hex digest
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        echo "neither sha256sum nor shasum is installed" >&2
        return 1
    fi
}

# Download, verify and install the release archive. Any failure returns 1.
install_release() {
    version=$1
    case $(uname -s) in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        *) say "unsupported OS $(uname -s)"; return 1 ;;
    esac
    case $(uname -m) in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) say "unsupported architecture $(uname -m)"; return 1 ;;
    esac
    base=${HERDR_SLEEP_RELEASE_BASE_URL:-$REPO_DEFAULT_BASE}
    name="herdr-sleep-$version-$os-$arch.tar.gz"
    url="$base/v$version/$name"

    tmp=$(mktemp -d)
    say "downloading $url"
    fetch "$url" >"$tmp/$name" || { say "download failed"; return 1; }
    fetch "$url.sha256" >"$tmp/$name.sha256" || { say "checksum download failed"; return 1; }

    say "verifying checksum"
    expected=$(cut -d ' ' -f 1 <"$tmp/$name.sha256" | head -n 1)
    actual=$(sha256_of "$tmp/$name") || return 1
    if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
        say "checksum mismatch"
        return 1
    fi

    say "extracting"
    mkdir "$tmp/x"
    tar -xzf "$tmp/$name" -C "$tmp/x" || { say "extraction failed"; return 1; }
    for b in $BINARIES; do
        found=$(find "$tmp/x" -type f -name "$b" | head -n 1)
        if [ -z "$found" ]; then
            say "$b is missing from the archive"
            return 1
        fi
    done
    for b in $BINARIES; do
        install_binary "$(find "$tmp/x" -type f -name "$b" | head -n 1)" "$b"
    done
    say "installed release $version into bin/"
}

build_from_source() {
    if ! command -v cargo >/dev/null 2>&1; then
        cat >&2 <<MSG
herdr-sleep build: failed.
No prebuilt binary could be installed ($1) and cargo is not available to build from source.
To fix it, either:
  - install Rust from https://rustup.rs and run the install again, or
  - download a release archive from https://github.com/Pepsit36/herdr-sleep/releases
    and extract herdr-sleep and herdr-sleep-ph into the bin/ directory of the plugin.
MSG
        exit 1
    fi
    say "building from source with cargo (this can take a few minutes)"
    cargo build --release --locked
    for b in $BINARIES; do
        install_binary "target/release/$b" "$b"
    done
    say "installed source build into bin/"
}

version=$(manifest_version)
if [ "${HERDR_SLEEP_FORCE_CARGO:-}" = 1 ]; then
    reason="HERDR_SLEEP_FORCE_CARGO is set"
elif [ -z "$version" ] || [ "$version" = 0.0.0 ]; then
    reason="unreleased checkout"
else
    if install_release "$version"; then
        exit 0
    fi
    reason="release $version unavailable"
fi
say "falling back to a source build ($reason)"
build_from_source "$reason"
