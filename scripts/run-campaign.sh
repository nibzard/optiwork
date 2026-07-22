#!/usr/bin/env bash
# ABOUTME: Run the regexbench optimization campaign end to end.
# ABOUTME: Builds, regenerates corpora + golden vectors, runs the correctness gates
# ABOUTME: (fmt/test/clippy/check), then drives each candidate through optikit-campaign
# ABOUTME: and finishes with held-out confirmations. Appends every entry to LOG.md.
#
# The ladder lives here (one optikit-campaign call per step). The keep/reject rule is
# frozen in the driver: a candidate is promoted only if it beats its baseline on BOTH
# corpora (lower 95% bound > 1). Every step measures against the naive start; BEST
# only records the last promoted candidate for the final summary.
set -euo pipefail

cd "$(dirname "$0")/.."

BENCH=./target/release/examples/bench
PAIRED=./target/release/optikit-paired
CAMPAIGN=./target/release/optikit-campaign
LOG=LOG.md

# Shared preregistered parameters. main is benign (cheap per byte) so we afford many
# sessions/blocks; pathological is catastrophic for naive, so sessions stay tiny.
MAIN_SESSIONS=30
MAIN_BLOCKS=8
PATH_SESSIONS=3
PATH_BLOCKS=6

echo "==> building (release)"
cargo build --release -p optikit-paired -p optikit-campaign
cargo build --release -p regexbench --example bench

echo "==> regenerating corpora + golden vectors"
cargo run -p regexbench --features oracle --example gen_golden -- corpora

echo "==> correctness gates: fmt / test / clippy"
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings

# Corpus byte counts (so --count matches exactly what the bench scans). Computed
# after the build and corpus regeneration so they can never go stale.
MAIN_COUNT=$("$BENCH" --count-of corpora/main.bin)
PATH_COUNT=$("$BENCH" --count-of corpora/pathological.bin)

# Last promoted candidate, echoed in the final summary. Starts at the naive baseline.
BEST=naive

run_step() {
  # run_step <id> <baseline_impl> <candidate_impl> <hypothesis> <order_seed> <seed> [--held-out]
  local id="$1" base="$2" cand="$3" hypothesis="$4" oseed="$5" seed="$6"
  shift 6
  echo "==> step: $base -> $cand  (id=$id order_seed=$oseed seed=$seed ${*:-})"
  if "$CAMPAIGN" \
    --bench "$BENCH" --paired "$PAIRED" \
    --baseline-impl "$base" --candidate-impl "$cand" \
    --id "$id" --hypothesis "$hypothesis" \
    --corpora-dir corpora --log "$LOG" \
    --main-count "$MAIN_COUNT" --main-sessions "$MAIN_SESSIONS" --main-blocks "$MAIN_BLOCKS" \
    --pathological-count "$PATH_COUNT" --pathological-sessions "$PATH_SESSIONS" --pathological-blocks "$PATH_BLOCKS" \
    --order-seed "$oseed" --seed "$seed" "$@"; then
    BEST="$cand"
    echo "==> promoted: $base -> $cand (best is now $BEST)"
  else
    echo "==> not promoted (best stays $BEST)"
  fi
}

# --- step 1: naive -> thompson ------------------------------------------------
# Pike VM: linear, no backtracking. Expected to be ~125000x faster on pathological
# but to REGRESS on main (per-byte thread-list overhead vs tight backtracking on
# benign inputs). The two-corpus rule surfaces that honestly → rejected.
run_step thompson naive thompson \
  "Pike VM bounds the live-thread set, removing exponential retry on alternation (catastrophic backtracking)." \
  1 42

# Held-out confirmation of the rejection: fresh seeds/order-seed not used above.
run_step thompson-heldout naive thompson \
  "Pre-registered confirmation of the pathological-corpus win with seeds/order-seed not used in exploration." \
  77 911 --held-out

# --- step 2: naive -> prefilter ----------------------------------------------
# Literal-prefix memchr prefilter, verified by the linear Thompson engine. The main
# corpus is dominated (by byte count) by one 8 KiB haystack scanned for "needle";
# naive walks it byte by byte, the prefilter SIMD-skips to the four hits. On
# pathological it delegates to Thompson, so it keeps the linear win. Expected: the
# first candidate promoted — faster on BOTH corpora.
run_step prefilter naive prefilter \
  "SIMD literal-prefix scan (memchr) jumps to candidate starts; anchored linear verify. Removes per-byte walk on literal-leading patterns." \
  1 42

# Held-out confirmation of the promotion: fresh seeds/order-seed.
run_step prefilter-heldout naive prefilter \
  "Pre-registered confirmation of the main+pathological win with seeds/order-seed not used in exploration." \
  77 911 --held-out

echo "==> campaign complete (final best=$BEST). See $LOG."
