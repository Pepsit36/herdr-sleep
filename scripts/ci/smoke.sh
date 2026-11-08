#!/bin/sh
# Check the binaries of a cross build: right format and machine, and, when the
# runner can execute them, that `--version` prints the expected version.
#
# Usage: smoke.sh TARGET_TRIPLE VERSION
set -eu

[ "$#" -eq 2 ] || { echo "usage: smoke.sh TARGET_TRIPLE VERSION" >&2; exit 2; }
target=$1
version=$2
dir="target/$target/release"

case $target in
    x86_64-unknown-linux-musl) expect='ELF 64-bit.*x86-64' ;;
    aarch64-unknown-linux-musl) expect='ELF 64-bit.*ARM aarch64' ;;
    x86_64-apple-darwin) expect='Mach-O 64-bit.*x86_64' ;;
    aarch64-apple-darwin) expect='Mach-O 64-bit.*arm64' ;;
    *) echo "smoke: unknown target $target" >&2; exit 2 ;;
esac

for binary in herdr-sleep herdr-sleep-ph; do
    description=$(file "$dir/$binary")
    echo "$description"
    echo "$description" | grep -Eq "$expect" || { echo "smoke: $binary is not $expect" >&2; exit 1; }
    case $target in
        *linux-musl) echo "$description" | grep -Eq 'statically linked|static-pie linked' \
            || { echo "smoke: $binary is not static" >&2; exit 1; } ;;
    esac
done

if [ "$target" = x86_64-unknown-linux-musl ]; then
    output=$("$dir/herdr-sleep" --version)
    echo "$output"
    case $output in
        *"$version"*) ;;
        *) echo "smoke: --version does not mention $version" >&2; exit 1 ;;
    esac
fi
