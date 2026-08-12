#!/bin/sh
# Install the latest vsorg release for macOS or Linux.
set -eu

repository='prashan-s/vsorg'
binary='vsorg'
install_dir="${VSORG_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
architecture="$(uname -m)"

case "$os:$architecture" in
  Darwin:arm64 | Darwin:aarch64) target='aarch64-apple-darwin' ;;
  Linux:x86_64) target='x86_64-unknown-linux-gnu' ;;
  Linux:arm64 | Linux:aarch64) target='aarch64-unknown-linux-gnu' ;;
  *)
    echo "Unsupported platform: $os $architecture" >&2
    echo 'Build from source with: cargo install --git https://github.com/prashan-s/vsorg vsorg' >&2
    exit 1
    ;;
esac

archive="${binary}-${target}.tar.gz"
base_url="https://github.com/${repository}/releases/latest/download"
temporary_dir="$(mktemp -d)"
cleanup() { rm -rf "$temporary_dir"; }
trap cleanup EXIT HUP INT TERM

curl --fail --silent --show-error --location --output "$temporary_dir/$archive" "$base_url/$archive"
curl --fail --silent --show-error --location --output "$temporary_dir/SHA256SUMS" "$base_url/SHA256SUMS"

expected_checksum="$(awk -v file="$archive" '$2 == file { print $1 }' "$temporary_dir/SHA256SUMS")"
if [ -z "$expected_checksum" ]; then
  echo "Checksum not found for $archive" >&2
  exit 1
fi

if command -v shasum >/dev/null 2>&1; then
  actual_checksum="$(shasum -a 256 "$temporary_dir/$archive" | awk '{ print $1 }')"
elif command -v sha256sum >/dev/null 2>&1; then
  actual_checksum="$(sha256sum "$temporary_dir/$archive" | awk '{ print $1 }')"
else
  echo 'Neither shasum nor sha256sum is available for checksum verification' >&2
  exit 1
fi

if [ "$expected_checksum" != "$actual_checksum" ]; then
  echo "Checksum verification failed for $archive" >&2
  exit 1
fi

tar -xzf "$temporary_dir/$archive" -C "$temporary_dir"
mkdir -p "$install_dir"
install -m 755 "$temporary_dir/$binary" "$install_dir/$binary"

echo "Installed $binary to $install_dir/$binary"
