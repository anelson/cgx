#!/usr/bin/env bash
#
# Install cgx (and optionally cargo-cgx) on a Unix runner for the `anelson/cgx`
# GitHub Action. Prefers prebuilt binaries; falls back to `cargo install`.
#
# Inputs (environment):
#   INPUT_VERSION     "latest" or "vX.Y.Z" (already normalized by action.yml)
#   INPUT_TARGET      target triple to force, or empty for native auto-detect
#   INPUT_CARGO_CGX   "true" to also install cargo-cgx
#   CGX_GITHUB_TOKEN  token for authenticated downloads (consumed by the
#                     cargo-dist installer and used as a curl auth header)
#
# Outputs (written to $GITHUB_OUTPUT): version, cgx-version, path

set -uo pipefail

REPO=anelson/cgx
VERSION="${INPUT_VERSION:-latest}"
TARGET="${INPUT_TARGET:-}"
WANT_CARGO_CGX="${INPUT_CARGO_CGX:-false}"

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
dest="$cargo_home/bin"
mkdir -p "$dest"

if [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

curl_dl() {
  # curl_dl <url> <output-path>
  if [ -n "${CGX_GITHUB_TOKEN:-}" ]; then
    curl --proto '=https' --tlsv1.2 --retry 10 -fSL \
      -H "Authorization: Bearer ${CGX_GITHUB_TOKEN}" -o "$2" "$1"
  else
    curl --proto '=https' --tlsv1.2 --retry 10 -fSL -o "$2" "$1"
  fi
}

verify_sha256() {
  # verify_sha256 <sidecar-file> (format: "<hex> *<filename>")
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$1"
  else
    shasum -a 256 -c "$1"
  fi
}

# Default path: let the cargo-dist installer auto-detect the platform, verify
# checksums, install to $dest, and add it to $GITHUB_PATH.
install_via_dist_installer() {
  # The cargo-dist installer treats CGX_INSTALL_DIR as a cargo-home root and
  # appends "/bin" itself. Point it at the cargo home root, not $dest (which
  # already ends in /bin), otherwise cgx lands in a nested .../bin/bin directory.
  export CGX_INSTALL_DIR="$cargo_home"
  export CGX_DISABLE_UPDATE=1
  export CGX_UNMANAGED_INSTALL=1
  curl --proto '=https' --tlsv1.2 --retry 10 -fsSL "$base/cgx-installer.sh" | sh || return 1
  if [ "$WANT_CARGO_CGX" = true ]; then
    curl --proto '=https' --tlsv1.2 --retry 10 -fsSL "$base/cargo-cgx-installer.sh" | sh || return 1
  fi
}

# Override path: the cargo-dist installer can't target a foreign triple, so fetch
# the exact archive, verify its .sha256 sidecar, and extract the binary. Unix
# archives are .tar.xz with the binary nested under "<name>-<target>/".
fetch_one() {
  # fetch_one <name>   (name is cgx or cargo-cgx)
  local name="$1"
  local archive="${name}-${TARGET}.tar.xz"
  local workdir
  workdir="$(mktemp -d)"
  (
    cd "$workdir" || exit 1
    curl_dl "$base/$archive" "$archive" || exit 1
    curl_dl "$base/$archive.sha256" "$archive.sha256" || exit 1
    verify_sha256 "$archive.sha256" || exit 1
    tar -xJf "$archive" || exit 1
    install -m 0755 "${name}-${TARGET}/${name}" "$dest/${name}" || exit 1
  )
  local rc=$?
  rm -rf "$workdir"
  return "$rc"
}

install_via_manual_download() {
  fetch_one cgx || return 1
  if [ "$WANT_CARGO_CGX" = true ]; then
    fetch_one cargo-cgx || return 1
  fi
}

source_fallback() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "::error::cgx: no prebuilt binary for this platform and no Rust toolchain (cargo) to build from source" >&2
    return 1
  fi
  local v=""
  [ "$VERSION" != latest ] && v="${VERSION#v}"

  # Honor an explicitly requested target so a source build produces the requested
  # architecture instead of silently building a host-native binary. The `+`
  # expansion keeps this safe under `set -u` when no target was requested.
  local target_args=()
  [ -n "$TARGET" ] && target_args=(--target "$TARGET")
  local cargo_args=(install --locked "${target_args[@]+"${target_args[@]}"}")

  if [ -n "$v" ]; then
    cargo "${cargo_args[@]}" cgx --version "$v" || return 1
  else
    cargo "${cargo_args[@]}" cgx || return 1
  fi

  if [ "$WANT_CARGO_CGX" = true ]; then
    if [ -n "$v" ]; then
      cargo "${cargo_args[@]}" cargo-cgx --version "$v" || return 1
    else
      cargo "${cargo_args[@]}" cargo-cgx || return 1
    fi
  fi
  return 0
}

ok=1
if [ -z "$TARGET" ]; then
  install_via_dist_installer || ok=0
else
  install_via_manual_download || ok=0
fi

if [ "$ok" -ne 1 ]; then
  echo "::warning::cgx: prebuilt install failed; building from source with 'cargo install --locked'" >&2
  source_fallback || exit 1
fi

# The cargo-dist installer already adds $dest to $GITHUB_PATH; do it for the
# manual/source paths too (idempotent).
case ":$PATH:" in
  *":$dest:"*) ;;
  *) [ -n "${GITHUB_PATH:-}" ] && echo "$dest" >> "$GITHUB_PATH" ;;
esac

cgx_bin="$dest/cgx"
[ -x "$cgx_bin" ] || cgx_bin="$(command -v cgx || echo "$dest/cgx")"
# `cgx --version` prints "cgx <version>" (optionally followed by " (<sha> <date>)"), so the
# version is the second field, not the last. Merge stderr into stdout (2>&1) because older
# releases (<=0.0.11) print --version to stderr; 2>/dev/null would then capture nothing.
cgx_version="$("$cgx_bin" --version 2>&1 | awk 'NR==1{print $2}')" || cgx_version=""

{
  echo "version=$VERSION"
  echo "cgx-version=$cgx_version"
  echo "path=$cgx_bin"
} >> "$GITHUB_OUTPUT"
