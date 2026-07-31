#!/usr/bin/env bash
#
# Install cgx (and optionally cargo-cgx) on a Unix runner for the `anelson/cgx`
# GitHub Action. Prefers prebuilt binaries; falls back to `cargo install`.
#
# Inputs (environment):
#   INPUT_VERSION              "latest" or "vX.Y.Z" (normalized by action.yml)
#   INPUT_TARGET               target triple to force, or empty for native detection
#   INPUT_CARGO_CGX            "true" to also install cargo-cgx
#   INPUT_VERIFY_ATTESTATIONS  "true" to require release and artifact attestations
#   CGX_GITHUB_TOKEN           token for authenticated GitHub requests
#
# Outputs (written to $GITHUB_OUTPUT): version, cgx-version, path

set -uo pipefail

REPO=anelson/cgx
SIGNER_WORKFLOW=anelson/cgx/.github/workflows/release.yml
VERSION="${INPUT_VERSION:-latest}"
REQUESTED_TARGET="${INPUT_TARGET:-}"
WANT_CARGO_CGX="${INPUT_CARGO_CGX:-false}"
VERIFY_ATTESTATIONS="${INPUT_VERIFY_ATTESTATIONS:-true}"

readonly PREBUILT_UNAVAILABLE=10
readonly SECURITY_FAILURE=20

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
dest="$cargo_home/bin"
mkdir -p "$dest"

case "$VERIFY_ATTESTATIONS" in
  true | false) ;;
  *)
    echo "::error::cgx: verify-attestations must be 'true' or 'false'" >&2
    exit 1
    ;;
esac

if [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

curl_dl() {
  if [ -n "${CGX_GITHUB_TOKEN:-}" ]; then
    curl --proto '=https' --tlsv1.2 --retry 10 -fSL \
      -H "Authorization: Bearer ${CGX_GITHUB_TOKEN}" -o "$2" "$1"
  else
    curl --proto '=https' --tlsv1.2 --retry 10 -fSL -o "$2" "$1"
  fi
}

verify_sha256() {
  local archive="$1"
  local sidecar="$2"
  local expected
  local actual

  expected="$(awk 'NR == 1 { print tolower($1) }' "$sidecar")"
  [ "${#expected}" -eq 64 ] || return 1
  case "$expected" in *[!0-9a-f]*) return 1 ;; esac

  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$archive" | awk '{ print tolower($1) }')"
  else
    actual="$(shasum -a 256 "$archive" | awk '{ print tolower($1) }')"
  fi
  [ "$actual" = "$expected" ]
}

security_failure() {
  echo "::error::cgx: $*" >&2
  return "$SECURITY_FAILURE"
}

resolve_verified_release() {
  local attempt
  local release_info
  local is_draft
  local is_immutable
  local release_resolved=0
  local args=(release view)

  if ! command -v gh >/dev/null 2>&1; then
    security_failure "GitHub CLI is required when verify-attestations is enabled"
    return "$?"
  fi
  if ! gh release verify-asset --help >/dev/null 2>&1 || \
    ! gh attestation verify --help >/dev/null 2>&1
  then
    security_failure "GitHub CLI does not support the required release and attestation verification commands"
    return "$?"
  fi

  [ -n "${CGX_GITHUB_TOKEN:-}" ] && export GH_TOKEN="$CGX_GITHUB_TOKEN"
  [ "$VERSION" != latest ] && args+=("$VERSION")
  args+=(--repo "$REPO" --json tagName,isDraft,isImmutable --jq '[.tagName, .isDraft, .isImmutable] | @tsv')
  for attempt in 1 2 3; do
    if release_info="$(gh "${args[@]}")"; then
      release_resolved=1
      break
    fi
    if [ "$attempt" -lt 3 ]; then sleep $((attempt * 2)); fi
  done
  if [ "$release_resolved" -ne 1 ]; then
    security_failure "failed to resolve GitHub release '$VERSION'"
    return "$?"
  fi

  IFS=$'\t' read -r RESOLVED_TAG is_draft is_immutable <<< "$release_info"
  if [ -z "$RESOLVED_TAG" ] || [ "$is_draft" != false ] || [ "$is_immutable" != true ]; then
    security_failure "release '$VERSION' is not a published immutable release"
    return "$?"
  fi
  base="https://github.com/$REPO/releases/download/$RESOLVED_TAG"
}

resolve_native_target() {
  local libc

  case "${RUNNER_OS:-}/${RUNNER_ARCH:-}" in
    Linux/X64)
      libc=gnu
      if ldd --version 2>&1 | grep -qi musl; then libc=musl; fi
      printf 'x86_64-unknown-linux-%s\n' "$libc"
      ;;
    Linux/ARM64)
      libc=gnu
      if ldd --version 2>&1 | grep -qi musl; then libc=musl; fi
      printf 'aarch64-unknown-linux-%s\n' "$libc"
      ;;
    macOS/X64) printf '%s\n' x86_64-apple-darwin ;;
    macOS/ARM64) printf '%s\n' aarch64-apple-darwin ;;
    *) return 1 ;;
  esac
}

is_unix_release_target() {
  case "$1" in
    aarch64-apple-darwin | x86_64-apple-darwin | \
      aarch64-unknown-linux-gnu | aarch64-unknown-linux-musl | \
      x86_64-unknown-linux-gnu | x86_64-unknown-linux-musl) return 0 ;;
    *) return 1 ;;
  esac
}

verify_with_retry() {
  local description="$1"
  shift
  local attempt

  for attempt in 1 2 3; do
    if "$@"; then return 0; fi
    if [ "$attempt" -lt 3 ]; then sleep $((attempt * 2)); fi
  done
  security_failure "$description"
}

install_verified_archives() {
  local target="$1"
  local workdir
  local name
  local archive
  local extract_dir
  local names=(cgx)
  [ "$WANT_CARGO_CGX" = true ] && names+=(cargo-cgx)

  workdir="$(mktemp -d)" || return "$PREBUILT_UNAVAILABLE"
  for name in "${names[@]}"; do
    archive="${name}-${target}.tar.xz"
    if ! curl_dl "$base/$archive" "$workdir/$archive" || \
      ! curl_dl "$base/$archive.sha256" "$workdir/$archive.sha256"
    then
      rm -rf "$workdir"
      return "$PREBUILT_UNAVAILABLE"
    fi
    if ! verify_sha256 "$workdir/$archive" "$workdir/$archive.sha256"; then
      rm -rf "$workdir"
      security_failure "SHA-256 verification failed for $archive"
      return "$?"
    fi
    verify_with_retry \
      "immutable-release verification failed for $archive" \
      gh release verify-asset "$RESOLVED_TAG" "$workdir/$archive" --repo "$REPO"
    local rc="$?"
    if [ "$rc" -ne 0 ]; then
      rm -rf "$workdir"
      return "$rc"
    fi
    verify_with_retry \
      "build-provenance verification failed for $archive" \
      gh attestation verify "$workdir/$archive" \
        --repo "$REPO" \
        --signer-workflow "$SIGNER_WORKFLOW" \
        --source-ref "refs/tags/$RESOLVED_TAG"
    rc="$?"
    if [ "$rc" -ne 0 ]; then
      rm -rf "$workdir"
      return "$rc"
    fi
  done

  for name in "${names[@]}"; do
    archive="${name}-${target}.tar.xz"
    extract_dir="$workdir/extract-$name"
    mkdir -p "$extract_dir" || {
      rm -rf "$workdir"
      return "$PREBUILT_UNAVAILABLE"
    }
    if ! tar -xJf "$workdir/$archive" -C "$extract_dir" || \
      ! install -m 0755 "$extract_dir/${name}-${target}/${name}" "$dest/${name}"
    then
      rm -rf "$workdir"
      return "$PREBUILT_UNAVAILABLE"
    fi
  done
  rm -rf "$workdir"
}

install_via_dist_installer() {
  # The cargo-dist installer appends /bin to CGX_INSTALL_DIR itself.
  export CGX_INSTALL_DIR="$cargo_home"
  export CGX_DISABLE_UPDATE=1
  export CGX_UNMANAGED_INSTALL=1
  curl --proto '=https' --tlsv1.2 --retry 10 -fsSL "$base/cgx-installer.sh" | sh || return 1
  if [ "$WANT_CARGO_CGX" = true ]; then
    curl --proto '=https' --tlsv1.2 --retry 10 -fsSL "$base/cargo-cgx-installer.sh" | sh || return 1
  fi
}

fetch_one_unverified() {
  local name="$1"
  local archive="${name}-${REQUESTED_TARGET}.tar.xz"
  local workdir
  workdir="$(mktemp -d)"
  (
    cd "$workdir" || exit 1
    curl_dl "$base/$archive" "$archive" || exit 1
    curl_dl "$base/$archive.sha256" "$archive.sha256" || exit 1
    verify_sha256 "$archive" "$archive.sha256" || exit 1
    tar -xJf "$archive" || exit 1
    install -m 0755 "${name}-${REQUESTED_TARGET}/${name}" "$dest/${name}" || exit 1
  )
  local rc=$?
  rm -rf "$workdir"
  return "$rc"
}

install_via_unverified_manual_download() {
  fetch_one_unverified cgx || return 1
  if [ "$WANT_CARGO_CGX" = true ]; then
    fetch_one_unverified cargo-cgx || return 1
  fi
}

source_fallback() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "::error::cgx: no prebuilt binary for this platform and no Rust toolchain (cargo) to build from source" >&2
    return 1
  fi
  local selected_version="${RESOLVED_TAG:-$VERSION}"
  local v=""
  [ "$selected_version" != latest ] && v="${selected_version#v}"
  # Honor an explicitly requested target in the source build too.
  local target_args=()
  [ -n "$REQUESTED_TARGET" ] && target_args=(--target "$REQUESTED_TARGET")
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
}

use_source_fallback=0
if [ "$VERIFY_ATTESTATIONS" = true ]; then
  RESOLVED_TAG=""
  resolve_verified_release || exit 1
  if [ -n "$REQUESTED_TARGET" ]; then
    selected_target="$REQUESTED_TARGET"
  elif ! selected_target="$(resolve_native_target)"; then
    use_source_fallback=1
  fi

  if [ "$use_source_fallback" -eq 0 ] && ! is_unix_release_target "$selected_target"; then
    use_source_fallback=1
  fi
  if [ "$use_source_fallback" -eq 0 ]; then
    if install_verified_archives "$selected_target"; then
      :
    else
      rc=$?
      if [ "$rc" -eq "$SECURITY_FAILURE" ]; then exit 1; fi
      use_source_fallback=1
    fi
  fi
else
  RESOLVED_TAG=""
  if [ -z "$REQUESTED_TARGET" ]; then
    install_via_dist_installer || use_source_fallback=1
  else
    install_via_unverified_manual_download || use_source_fallback=1
  fi
fi

if [ "$use_source_fallback" -eq 1 ]; then
  echo "::warning::cgx: prebuilt install failed; building from source with 'cargo install --locked'" >&2
  source_fallback || exit 1
fi

case ":$PATH:" in
  *":$dest:"*) ;;
  *) [ -n "${GITHUB_PATH:-}" ] && echo "$dest" >> "$GITHUB_PATH" ;;
esac

cgx_bin="$dest/cgx"
[ -x "$cgx_bin" ] || cgx_bin="$(command -v cgx || echo "$dest/cgx")"
# Older releases print --version to stderr; the version is the second field.
cgx_version="$("$cgx_bin" --version 2>&1 | awk 'NR == 1 { print $2 }')" || cgx_version=""

{
  echo "version=$VERSION"
  echo "cgx-version=$cgx_version"
  echo "path=$cgx_bin"
} >> "$GITHUB_OUTPUT"
