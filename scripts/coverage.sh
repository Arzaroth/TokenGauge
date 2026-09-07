#!/usr/bin/env bash
# Line coverage for the workspace, via cargo-llvm-cov.
#
# --all-features matches CI's test job: without it the self-update module is
# compiled out and reads as uncovered rather than as absent.
#
#   scripts/coverage.sh              summary, then the files with the most
#                                    uncovered lines
#   scripts/coverage.sh --html       write and open target/llvm-cov/html
#   scripts/coverage.sh --lcov       write target/llvm-cov/lcov.info
#   scripts/coverage.sh -- -p tokengauge-core
#                                    everything after -- goes to cargo-llvm-cov
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TOP="${TOKENGAUGE_COVERAGE_TOP:-15}"

if [[ -t 1 ]]; then B="\033[0;34m"; G="\033[0;32m"; Y="\033[0;33m"; R="\033[0;31m"; Z="\033[0m"; else B=""; G=""; Y=""; R=""; Z=""; fi
info()    { printf '%b\n' "${B}$*${Z}"; }
success() { printf '%b\n' "${G}$*${Z}"; }
warn()    { printf '%b\n' "${Y}$*${Z}"; }
fail()    { printf '%b\n' "${R}$*${Z}" >&2; }

MODE=summary
EXTRA=()
while [[ $# -gt 0 ]]; do
  case "$1" in
  --html) MODE=html ;;
  --lcov) MODE=lcov ;;
  --open) MODE=html ;;
  --) shift; EXTRA=("$@"); break ;;
  -h | --help)
    sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'
    exit 0
    ;;
  *)
    fail "unknown option: $1 (pass cargo-llvm-cov flags after --)"
    exit 1
    ;;
  esac
  shift
done

command -v cargo >/dev/null 2>&1 || { fail "cargo not found - install Rust to build."; exit 1; }

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  fail "cargo-llvm-cov not found."
  warn "  cargo install cargo-llvm-cov"
  exit 1
fi

# llvm-cov reads the profile data with the toolchain's own llvm-tools; a
# rustup install without that component fails deep inside the run instead.
if ! rustc --print target-libdir >/dev/null 2>&1 ||
  ! find "$(rustc --print sysroot)" -name 'llvm-profdata*' -print -quit 2>/dev/null | grep -q .; then
  fail "llvm-tools-preview is missing from the active toolchain."
  warn "  rustup component add llvm-tools-preview"
  exit 1
fi

cd "$REPO_DIR"

# tokengauge-waybar is Linux-only, so --workspace cannot build on the other two
# hosts the project supports. Elsewhere, cover what those hosts can: the tray's
# GUI is cfg(windows)-gated and CI's Windows job is the authority on it.
if [[ "$(uname -s)" == "Linux" ]]; then
  SCOPE=(--workspace)
else
  SCOPE=(-p tokengauge-core -p tokengauge-tui)
  warn "$(uname -s): covering core and tui only - the waybar crate is Linux-only."
fi

case "$MODE" in
html)
  info "Running coverage (html)..."
  cargo llvm-cov "${SCOPE[@]}" --all-features --html --open "${EXTRA[@]+"${EXTRA[@]}"}"
  success "Report at target/llvm-cov/html/index.html"
  ;;
lcov)
  info "Running coverage (lcov)..."
  cargo llvm-cov "${SCOPE[@]}" --all-features --lcov --output-path target/llvm-cov/lcov.info "${EXTRA[@]+"${EXTRA[@]}"}"
  success "Report at target/llvm-cov/lcov.info"
  ;;
summary)
  info "Running coverage..."
  cargo llvm-cov "${SCOPE[@]}" --all-features --summary-only "${EXTRA[@]+"${EXTRA[@]}"}"

  # The summary is alphabetical, which buries the gaps. Re-read the same
  # profile data - `report` reuses it and runs no tests - and rank by how many
  # uncovered lines each file carries, since that is the size of the job.
  echo
  info "Most uncovered lines:"
  # `report` takes no feature flags - it re-reads the run above, it does not
  # configure one.
  cargo llvm-cov report --summary-only |
    awk -v top="$TOP" '
      /^[a-zA-Z].*%/ && $1 != "TOTAL" {
        seen++
        if ($9 + 0 > 0) {
          cover = $10; sub(/%$/, "", cover)
          rows[n++] = sprintf("  %6d uncovered  %6.2f%%  %s", $9, cover, $1)
        }
      }
      END {
        # A summary with no file rows in it means the format moved; one whose
        # rows all read zero means the job is done. Only the first is a fault.
        if (seen == 0) { print "  (no rows parsed - llvm-cov summary format changed?)"; exit 1 }
        if (n == 0) { print "  (nothing uncovered)"; exit 0 }
        # Sort descending by the leading uncovered count.
        for (i = 0; i < n; i++)
          for (j = i + 1; j < n; j++)
            if ((rows[j] + 0) > (rows[i] + 0)) { t = rows[i]; rows[i] = rows[j]; rows[j] = t }
        for (i = 0; i < n && i < top; i++) print rows[i]
      }'
  ;;
esac
