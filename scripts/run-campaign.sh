#!/usr/bin/env bash
# ABOUTME: Build and run the frozen regex showcase through the campaign lifecycle.
# ABOUTME: Results go to a new, self-contained run directory; inputs are never regenerated.
set -euo pipefail

cd "$(dirname "$0")/.."

if (( $# > 1 )); then
  echo "usage: $0 [new-run-directory]" >&2
  exit 2
fi

SPEC=campaigns/regex-demo.json
BENCH=target/release/examples/bench
ARTIFACT_DIR=target/release/optiwork-artifacts
ARTIFACT_LOCK=target/regex-demo-campaign.lock
RUN_DIR=${1:-"target/runs/regex-demo-$(date -u +%Y%m%dT%H%M%SZ)-$$"}

if [[ -e "$RUN_DIR" ]]; then
  echo "run directory already exists: $RUN_DIR" >&2
  exit 2
fi

mkdir -p target
if ! mkdir "$ARTIFACT_LOCK"; then
  echo "another regex demo campaign is running, or a stale lock remains: $ARTIFACT_LOCK" >&2
  exit 2
fi
release_artifact_lock() {
  rmdir "$ARTIFACT_LOCK"
}
trap release_artifact_lock EXIT

echo "==> verifying source and tests"
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings

echo "==> building immutable campaign artifacts"
cargo build --release --locked -p optikit-paired -p optikit-campaign
cargo build --release --locked -p regexbench --example bench
mkdir -p "$ARTIFACT_DIR"
install -m 0755 "$BENCH" "$ARTIFACT_DIR/regex-naive-bench"
install -m 0755 "$BENCH" "$ARTIFACT_DIR/regex-thompson-bench"
install -m 0755 "$BENCH" "$ARTIFACT_DIR/regex-prefilter-bench"
install -m 0755 "$BENCH" "$ARTIFACT_DIR/regex-lazy-dfa-bench"
install -m 0755 "$BENCH" "$ARTIFACT_DIR/regex-flat-dfa-bench"

for input in \
  corpora/main.bin \
  corpora/main.golden \
  corpora/pathological.bin \
  corpora/pathological.golden
do
  if [[ ! -f "$input" ]]; then
    echo "frozen campaign input is missing: $input" >&2
    exit 2
  fi
done

echo "==> running campaign: $RUN_DIR"
target/release/optikit-campaign --spec "$SPEC" --run-dir "$RUN_DIR"

echo "==> campaign complete"
echo "report: $RUN_DIR/report.md"
echo "events: $RUN_DIR/events.jsonl"
echo "state:  $RUN_DIR/state.json"
