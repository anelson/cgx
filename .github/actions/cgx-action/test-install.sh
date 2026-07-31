#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
installer="$script_dir/install.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT

fixtures="$test_root/fixtures"
mock_bin="$test_root/bin"
mkdir -p \
  "$fixtures/cgx-x86_64-unknown-linux-gnu" \
  "$fixtures/cargo-cgx-x86_64-unknown-linux-gnu" \
  "$mock_bin"

cat > "$fixtures/cgx-x86_64-unknown-linux-gnu/cgx" <<'EOF'
#!/usr/bin/env sh
echo "cgx 0.1.0"
EOF
chmod +x "$fixtures/cgx-x86_64-unknown-linux-gnu/cgx"
tar -cJf "$fixtures/cgx-x86_64-unknown-linux-gnu.tar.xz" \
  -C "$fixtures" cgx-x86_64-unknown-linux-gnu
sha256sum "$fixtures/cgx-x86_64-unknown-linux-gnu.tar.xz" \
  > "$fixtures/cgx-x86_64-unknown-linux-gnu.tar.xz.sha256"

cat > "$fixtures/cargo-cgx-x86_64-unknown-linux-gnu/cargo-cgx" <<'EOF'
#!/usr/bin/env sh
echo "cargo-cgx 0.1.0"
EOF
chmod +x "$fixtures/cargo-cgx-x86_64-unknown-linux-gnu/cargo-cgx"
tar -cJf "$fixtures/cargo-cgx-x86_64-unknown-linux-gnu.tar.xz" \
  -C "$fixtures" cargo-cgx-x86_64-unknown-linux-gnu
sha256sum "$fixtures/cargo-cgx-x86_64-unknown-linux-gnu.tar.xz" \
  > "$fixtures/cargo-cgx-x86_64-unknown-linux-gnu.tar.xz.sha256"
cat > "$fixtures/cgx-installer.sh" <<'EOF'
#!/usr/bin/env sh
set -eu
mkdir -p "$CGX_INSTALL_DIR/bin"
cp "$MOCK_CGX" "$CGX_INSTALL_DIR/bin/cgx"
chmod +x "$CGX_INSTALL_DIR/bin/cgx"
EOF

cat > "$mock_bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
output=""
url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      output="$2"
      shift 2
      ;;
    *)
      url="$1"
      shift
      ;;
  esac
done
printf '%s\n' "$url" >> "$CURL_LOG"
asset="${url##*/}"
if [ "${CURL_FAIL_ARCHIVE:-false}" = true ] && [[ "$asset" == *.tar.xz ]]; then
  exit 22
fi
if [ -n "$output" ]; then
  if [ "${CURL_BAD_CHECKSUM:-false}" = true ] && [[ "$asset" == *.sha256 ]]; then
    printf '%064d  %s\n' 0 "${asset%.sha256}" > "$output"
  else
    cp "$FIXTURES/$asset" "$output"
  fi
else
  cat "$FIXTURES/$asset"
fi
EOF
chmod +x "$mock_bin/curl"

cat > "$mock_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$GH_LOG"
case "$1 $2 ${3:-}" in
  "release verify-asset --help" | "attestation verify --help")
    [ "${GH_TOO_OLD:-false}" != true ]
    ;;
  "release view "*)
    if [ "${GH_RELEASE_MUTABLE:-false}" = true ]; then
      printf 'v0.1.0\tfalse\tfalse\n'
    else
      printf 'v0.1.0\tfalse\ttrue\n'
    fi
    ;;
  "release verify-asset "*)
    [ "${GH_FAIL_RELEASE:-false}" != true ]
    ;;
  "attestation verify "*)
    [ "${GH_FAIL_ATTESTATION:-false}" != true ]
    ;;
  *) exit 64 ;;
esac
EOF
chmod +x "$mock_bin/gh"

cat > "$mock_bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$CARGO_LOG"
name=cgx
for arg in "$@"; do
  if [ "$arg" = cargo-cgx ]; then name=cargo-cgx; fi
done
mkdir -p "$CARGO_HOME/bin"
cat > "$CARGO_HOME/bin/$name" <<SCRIPT
#!/usr/bin/env sh
echo "$name 0.1.0"
SCRIPT
chmod +x "$CARGO_HOME/bin/$name"
EOF
chmod +x "$mock_bin/cargo"

cat > "$mock_bin/sleep" <<'EOF'
#!/usr/bin/env sh
exit 0
EOF
chmod +x "$mock_bin/sleep"

new_case() {
  case_dir="$test_root/$1"
  mkdir -p "$case_dir"
  : > "$case_dir/curl.log"
  : > "$case_dir/gh.log"
  : > "$case_dir/cargo.log"
  : > "$case_dir/output"
  : > "$case_dir/path"
}

run_case() {
  env \
    PATH="$mock_bin:$PATH" \
    FIXTURES="$fixtures" \
    MOCK_CGX="$fixtures/cgx-x86_64-unknown-linux-gnu/cgx" \
    CURL_LOG="$case_dir/curl.log" \
    GH_LOG="$case_dir/gh.log" \
    CARGO_LOG="$case_dir/cargo.log" \
    CARGO_HOME="$case_dir/cargo-home" \
    GITHUB_OUTPUT="$case_dir/output" \
    GITHUB_PATH="$case_dir/path" \
    RUNNER_OS=Linux \
    RUNNER_ARCH=X64 \
    INPUT_VERSION=latest \
    INPUT_TARGET="${INPUT_TARGET:-}" \
    INPUT_CARGO_CGX="${INPUT_CARGO_CGX:-false}" \
    INPUT_VERIFY_ATTESTATIONS="${INPUT_VERIFY_ATTESTATIONS:-true}" \
    GH_RELEASE_MUTABLE="${GH_RELEASE_MUTABLE:-false}" \
    GH_FAIL_RELEASE="${GH_FAIL_RELEASE:-false}" \
    GH_FAIL_ATTESTATION="${GH_FAIL_ATTESTATION:-false}" \
    GH_TOO_OLD="${GH_TOO_OLD:-false}" \
    CURL_BAD_CHECKSUM="${CURL_BAD_CHECKSUM:-false}" \
    CURL_FAIL_ARCHIVE="${CURL_FAIL_ARCHIVE:-false}" \
    bash "$installer"
}

new_case verified
run_case
test -x "$case_dir/cargo-home/bin/cgx"
grep -Fq "release verify-asset v0.1.0" "$case_dir/gh.log"
grep -Fq -- "--signer-workflow anelson/cgx/.github/workflows/release.yml" "$case_dir/gh.log"
grep -Fq -- "--source-ref refs/tags/v0.1.0" "$case_dir/gh.log"
if grep -Fq installer.sh "$case_dir/curl.log"; then exit 1; fi

new_case cargo-cgx
INPUT_CARGO_CGX=true run_case
test -x "$case_dir/cargo-home/bin/cgx"
test -x "$case_dir/cargo-home/bin/cargo-cgx"
grep -Fq "release verify-asset v0.1.0" "$case_dir/gh.log"
grep -Fq cargo-cgx-x86_64-unknown-linux-gnu.tar.xz "$case_dir/gh.log"

new_case dist-installer
INPUT_VERIFY_ATTESTATIONS=false run_case
test -x "$case_dir/cargo-home/bin/cgx"
grep -Fq cgx-installer.sh "$case_dir/curl.log"
test ! -s "$case_dir/gh.log"

new_case explicit-target
INPUT_VERIFY_ATTESTATIONS=false INPUT_TARGET=x86_64-unknown-linux-gnu run_case
test -x "$case_dir/cargo-home/bin/cgx"
grep -Fq cgx-x86_64-unknown-linux-gnu.tar.xz "$case_dir/curl.log"
if grep -Fq installer.sh "$case_dir/curl.log"; then exit 1; fi
test ! -s "$case_dir/gh.log"

new_case mutable
if GH_RELEASE_MUTABLE=true run_case; then exit 1; fi
test ! -e "$case_dir/cargo-home/bin/cgx"
test ! -s "$case_dir/cargo.log"

new_case bad-checksum
if CURL_BAD_CHECKSUM=true run_case; then exit 1; fi
test ! -e "$case_dir/cargo-home/bin/cgx"
test ! -s "$case_dir/cargo.log"

new_case bad-attestation
if GH_FAIL_ATTESTATION=true run_case; then exit 1; fi
test ! -e "$case_dir/cargo-home/bin/cgx"
test ! -s "$case_dir/cargo.log"

new_case bad-release-attestation
if GH_FAIL_RELEASE=true run_case; then exit 1; fi
test ! -e "$case_dir/cargo-home/bin/cgx"
test ! -s "$case_dir/cargo.log"

new_case old-gh
if GH_TOO_OLD=true run_case; then exit 1; fi
test ! -e "$case_dir/cargo-home/bin/cgx"
test ! -s "$case_dir/cargo.log"

new_case source-fallback
CURL_FAIL_ARCHIVE=true run_case
test -x "$case_dir/cargo-home/bin/cgx"
grep -Fq -- "--version 0.1.0" "$case_dir/cargo.log"

echo "cgx action Unix installer tests passed"
