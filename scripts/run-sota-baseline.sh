#!/usr/bin/env bash
# ABOUTME: Measure the same-machine SOTA baseline: the real Rust `regex` crate on
# ABOUTME: optiwork's exact corpus. Builds an oracle-feature bench (regex_crate) and
# ABOUTME: a no-oracle bench (prefilter) in isolated target dirs, runs the equivalence
# ABOUTME: gate, an absolute timed reading, and a paired A/B expressing the gap to our
# ABOUTME: best impl as a 95% lower-bound ratio. The regex crate is the oracle, so
# ABOUTME: regex_crate is an external ceiling — never an optimization candidate.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

prefilter_bin=target/cmp/prefilter/release/examples/bench
sota_bin=target/cmp/regex_crate/release/examples/bench
paired=target/release/optikit-paired

for stem in main pathological; do
    for ext in bin golden; do
        if [[ ! -f "corpora/$stem.$ext" ]]; then
            echo "missing corpora/$stem.$ext (run gen_golden first)" >&2
            exit 2
        fi
    done
done

echo "==> building isolated benches (prefilter: no oracle, regex_crate: oracle)"
CARGO_TARGET_DIR=target/cmp/prefilter cargo build --release -p regexbench --example bench
CARGO_TARGET_DIR=target/cmp/regex_crate cargo build --release --features oracle -p regexbench --example bench
cargo build --release -p optikit-paired

main_bytes=$("$prefilter_bin" --count-of corpora/main.bin)
path_bytes=$("$prefilter_bin" --count-of corpora/pathological.bin)

gb() { awk -v b="$1" 'BEGIN { printf "%.2f", b / 1e9 }'; }

echo
echo "==> equivalence gate: regex_crate == oracle (both corpora)"
"$sota_bin" --check corpora/main.golden --impl regex_crate --corpus corpora/main.bin \
    --optiwork-gate-artifact-id sota --optiwork-gate-workload-id main
"$sota_bin" --check corpora/pathological.golden --impl regex_crate --corpus corpora/pathological.bin \
    --optiwork-gate-artifact-id sota --optiwork-gate-workload-id pathological

echo
echo "==> absolute throughput: regex_crate (bytes/s -> GB/s)"
main_ips=$("$sota_bin" --measure scan --impl regex_crate --corpus corpora/main.bin \
    --seed 42 --sessions 2000 --count "$main_bytes" \
    | sed -n 's/.*items_per_second=\([0-9.]*\).*/\1/p')
path_ips=$("$sota_bin" --measure scan --impl regex_crate --corpus corpora/pathological.bin \
    --seed 42 --sessions 100000 --count "$path_bytes" \
    | sed -n 's/.*items_per_second=\([0-9.]*\).*/\1/p')
printf 'main          regex_crate items_per_second=%s (%s GB/s)\n' "$main_ips" "$(gb "$main_ips")"
printf 'pathological  regex_crate items_per_second=%s (%s GB/s)\n' "$path_ips" "$(gb "$path_ips")"

echo
echo "==> paired A/B: prefilter (baseline) vs regex_crate (candidate), 95% lower bound"
"$paired" --baseline "$prefilter_bin" --candidate "$sota_bin" --measure scan \
    --count "$main_bytes" --sessions 200 --blocks 16 --seed 42 \
    --baseline-args "--impl prefilter --corpus corpora/main.bin" \
    --candidate-args "--impl regex_crate --corpus corpora/main.bin" \
    | grep '^RESULT' | sed 's/^/main          /'
"$paired" --baseline "$prefilter_bin" --candidate "$sota_bin" --measure scan \
    --count "$path_bytes" --sessions 5000 --blocks 16 --seed 42 \
    --baseline-args "--impl prefilter --corpus corpora/pathological.bin" \
    --candidate-args "--impl regex_crate --corpus corpora/pathological.bin" \
    | grep '^RESULT' | sed 's/^/pathological  /'
