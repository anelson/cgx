#!/usr/bin/env bash
#
# Warm cgx's cache for the `anelson/cgx` GitHub Action by prefetching tools after
# install. Any prefetch failure is logged as a GitHub warning and NEVER fails the
# job; partial successes remain cached.
#
# Inputs (environment):
#   INPUT_PREFETCH_ALL  "true" to run `cgx --prefetch-all`
#   INPUT_PREFETCH      newline-separated crate specs for `cgx --prefetch`
#   GITHUB_TOKEN        used by cgx at runtime to authenticate GitHub lookups

set -uo pipefail

# Locate cgx. The install step appended $CARGO_HOME/bin to $GITHUB_PATH, which
# applies to this (subsequent) step; fall back to well-known locations anyway.
cgx_bin="$(command -v cgx || true)"
if [ -z "$cgx_bin" ]; then
  for cand in "${CARGO_HOME:-$HOME/.cargo}/bin/cgx" "$HOME/.cargo/bin/cgx"; do
    if [ -x "$cand" ]; then
      cgx_bin="$cand"
      break
    fi
  done
fi
if [ -z "$cgx_bin" ]; then
  echo "::warning title=cgx prefetch::skipped; cgx binary not found on PATH" >&2
  exit 0
fi

warn() { echo "::warning title=cgx prefetch::$*" >&2; }

if [ "${INPUT_PREFETCH_ALL:-false}" = true ]; then
  echo "cgx: prefetching all configured tools (--prefetch-all)"
  "$cgx_bin" --prefetch-all || warn "--prefetch-all reported failures; see the log above. Continuing."
fi

# Prefetch each non-empty, non-comment line of the prefetch input. `read` (with
# the default IFS) trims surrounding whitespace; each line is one crate spec.
if [ -n "${INPUT_PREFETCH:-}" ]; then
  while read -r spec; do
    [ -z "$spec" ] && continue
    case "$spec" in
      \#*) continue ;;
    esac
    echo "cgx: prefetching $spec"
    "$cgx_bin" --prefetch "$spec" || warn "failed to prefetch '$spec'; see the log above. Continuing."
  done <<< "${INPUT_PREFETCH}"
fi

exit 0
