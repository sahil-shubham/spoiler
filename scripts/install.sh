#!/bin/sh
# Installs the prebuilt spoiler binary for this machine from GitHub Releases.
#
#   curl -fsSL https://spoiler.sh/install | sh
#
# SPOILER_VERSION      release tag to install, such as v0.1.0 (default: the latest release)
# SPOILER_INSTALL_DIR  directory for the binary (default: ~/.local/bin)
#
# Linux gets the static musl build, which runs on any distribution. The archive's sha256 is
# checked before anything is installed. Everything runs inside main, so a truncated download of
# this script executes nothing.
set -eu

main() {
  repo=https://github.com/sahil-shubham/spoiler
  version=${SPOILER_VERSION:-latest}
  dir=${SPOILER_INSTALL_DIR:-$HOME/.local/bin}

  case $(uname -s) in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-musl ;;
    *) fail "no prebuilt binary for $(uname -s); see $repo#install" ;;
  esac
  case $(uname -m) in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) fail "no prebuilt binary for $(uname -m); see $repo#install" ;;
  esac
  # An x86_64 shell under Rosetta still runs on Apple silicon: install the native build.
  if [ "$os" = apple-darwin ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
    arch=aarch64
  fi

  if [ "$version" = latest ]; then
    base=$repo/releases/latest/download
  else
    base=$repo/releases/download/$version
  fi
  if command -v sha256sum > /dev/null; then
    sum="sha256sum"
  elif command -v shasum > /dev/null; then
    sum="shasum -a 256"
  else
    fail "sha256sum or shasum is needed to verify the download"
  fi
  command -v curl > /dev/null || fail "curl is needed to download the binary"

  name=spoiler-$arch-$os
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  echo "Downloading $name ($version)" >&2
  fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz"
  fetch "$base/$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256"
  (cd "$tmp" && $sum -c "$name.tar.gz.sha256" > /dev/null) || fail "checksum mismatch for $name.tar.gz"
  tar -xzf "$tmp/$name.tar.gz" -C "$tmp"

  # Copy beside the target, then rename over it, so a running spoiler is replaced atomically.
  mkdir -p "$dir"
  cp "$tmp/$name/spoiler" "$dir/.spoiler.tmp"
  chmod 755 "$dir/.spoiler.tmp"
  mv -f "$dir/.spoiler.tmp" "$dir/spoiler"
  echo "Installed $("$dir/spoiler" --version) to $dir/spoiler" >&2
  case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "Add $dir to PATH to run spoiler." >&2 ;;
  esac
}

fetch() {
  curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2" || fail "could not download $1"
}

fail() {
  echo "spoiler install: $*" >&2
  exit 1
}

main "$@"
