# Benchmark-Guided Optimization Loop

Use fixed-work paired measurements for optimization decisions. The primary
metric is **bytes scanned per second** through one implementation's `find_all`
over a fixed `(pattern, input)` corpus: pattern compilation and process startup
are outside the timed region; the full corpus scan is inside it.

`output_bytes` (total bytes covered by reported matches) is emitted as a
diagnostic sidecar. It is not a substitute for the bytes-scanned result.

A prebuilt binary from before the `optiwork-fixed-v1` protocol cannot participate
in this design; first apply the benchmark-protocol commit to both comparison
revisions. The runner rejects any record with the wrong version or mode.

## Freeze the experiment

1. Work in an isolated branch or worktree and keep unrelated changes intact.
2. Commit benchmark changes separately, then build baseline and candidate from
   revisions that both contain the same benchmark protocol.
3. Choose corpora, count, timed sessions, work seeds, the order seed, confidence
   rule, and the maximum number of candidates **before** measuring a candidate.
4. Keep machine conditions stable. Do not run benchmark series concurrently.
5. Capture deterministic output and run the correctness gates:

   ```sh
   cargo fmt --all -- --check
   cargo test --all-targets
   cargo clippy --all-targets -- -D warnings
   ```

## Fixed-work measurements

Build the bench binary once per impl in separate target directories so both stay
available. The runner shells out and parses one `optiwork-fixed-v1` record per
process, so it cannot tell the two binaries apart:

```sh
CARGO_TARGET_DIR=/tmp/optiwork-a cargo build --release -p regexbench --example bench
CARGO_TARGET_DIR=/tmp/optiwork-b cargo build --release -p regexbench --example bench
cargo build --release -p optikit-paired
```

A fixed run scans the entire corpus through one impl's `find_all` once per
session, `sessions` times, after one untimed warmup. `--count` must equal the
corpus byte count exactly (the bench checks). Each session's work is the same
scan, but a seed-derived permutation reorders the `(pattern, input)` pairs so
every session has fresh branch and memory behavior — the match set is invariant
under pair reordering, so this changes timing only, never output.

```sh
# one observation, standalone
/path/to/bench --measure scan --impl thompson \
  --corpus corpora/main.bin --seed 42 --sessions 30 --count 8377
```

The record carries version, mode, seed, count, sessions, requested/completed
work, attempts, elapsed nanoseconds, throughput, and matched byte count.

## Reproducible PGO training

Build an instrumented bench, train it across `impls × corpora`, and build the
profile-optimized bench with one isolated command:

```sh
scripts/build-pgo.sh <unique-run-name>
```

The script trains each (impl, corpus) pair with explicit fixed-work sessions.
`naive` is omitted from the pathological corpus — it is exponential there and
would never finish a training run. Training counts, session counts, seeds,
source revision/state, tool versions, and the training matrix are recorded in
`target/pgo/<run-name>/manifest.txt`. Build-time profiles are discarded, and the
merge fails unless every planned runtime profile was produced. Treat the
optimized bench as another candidate and evaluate it with the same paired
protocol; do not compare build durations.

## Calibrate noise with A/A

Run the same prebuilt binary under both labels before testing candidates. The
runner generates its entire ABBA/BAAB schedule from `--order-seed` and prints the
plan before the first process starts. Repeating `--seed` cycles a fixed,
preregistered base-seed set across blocks.

```sh
target/release/optikit-paired \
  --aa target/release/examples/bench \
  --measure scan \
  --subject-args "--impl thompson --corpus corpora/main.bin" \
  --count 8377 --sessions 30 --blocks 16 --order-seed 731 \
  --seed 42 --seed 314159 --seed 271828 \
  --target-speedup 3 | tee /tmp/optiwork-aa.log
```

`CALIBRATION` reports the observed block log-ratio standard deviation and an
approximate block count for 80% power at the requested speedup. Choose and
record the A/B block count before seeing candidate results; normally use at least
8–16 blocks. A/A is a noise estimate, not evidence of a performance change.

## Compare candidates (A/B) on both corpora

```sh
target/release/optikit-paired \
  --baseline target/release/examples/bench \
  --candidate target/release/examples/bench \
  --measure scan \
  --baseline-args "--impl naive --corpus corpora/main.bin" \
  --candidate-args "--impl prefilter --corpus corpora/main.bin" \
  --count 8377 --sessions 30 --blocks 8 --order-seed 9127 \
  --seed 42 | tee /tmp/optiwork-ab-main.log
```

Run the same frozen design for `pathological`. The two corpora carry real weight
because the wins concentrate differently: `thompson`/`prefilter` win huge on
pathological, but `naive`'s tight backtracking is competitive on benign `main`
input. The keep rule below requires winning on **both**.

Each block contains two A and two B observations. The runner computes one paired
log-throughput ratio per block, then reports:

- the geometric candidate/baseline speedup;
- the standard deviation of block log ratios;
- a Student-t 95% one-sided lower confidence bound.

Every complete, valid preregistered block is retained, including unusually fast
or slow blocks. A block is invalid only when a process cannot start, exits
unsuccessfully, emits a malformed record, or reports different requested work.
Invalid blocks are logged, never replaced, and cause a failing runner exit.

The per-candidate 95% result is labeled `scope=exploratory_per_candidate`. It is
a screening result, not a familywise-error-controlled claim across a campaign.
Use its lower bound to choose candidates, then reserve the confirmatory claim for
fresh held-out data.

## Equivalence gate (correctness before timing)

Every candidate must produce the **exact same span set** as the oracle (the Rust
`regex` crate, run offline to build `*.golden`). The gate runs before any timing
and is fail-closed:

```sh
bench --check corpora/main.golden --impl <cand> --corpus corpora/main.bin
bench --check corpora/pathological.golden --impl <cand> --corpus corpora/pathological.bin
```

A mismatch exits nonzero and the driver rejects the candidate before timing. This
mirrors fenrin's byte-`cmp` gate: a speed difference must be provably
performance, not a semantics change.

## Each candidate

1. State one concrete hypothesis and the work it should remove.
2. Make one reversible change.
3. Run formatting, tests, Clippy, and the equivalence gate on both corpora.
4. Run the frozen paired designs on **both** corpora.
5. Keep or reject using only the frozen rule below. Do not add runs, remove
   observations, or change the analysis after seeing the result.
6. Log the hypothesis, commit, full `PLAN`/`RESULT` records, gate results, and
   decision. An accepted candidate becomes the next baseline.

**Frozen keep/reject rule:** promote iff all gates passed AND the 95% one-sided
lower bound (`lower_95_one_sided_ratio`) is `> 1.0` on `main` **and** on
`pathological`. A win on only one corpus is a rejection — it signals a
specialization the other corpus penalizes.

For an **intentional semantics change** (the reserved demo: switching from
leftmost-first to POSIX leftmost-longest), the span-set gate is *expected* to
diverge. Define the new oracle and a separate multi-case quality check before
timing, and never use throughput to waive correctness.

## Held-out confirmation

After the last accepted candidate, use fresh work seeds and a new order seed for
one optimized-versus-start comparison. The driver's `--held-out` flag labels the
record `scope=held_out_confirmation`:

```sh
optikit-campaign ... --baseline-impl naive --candidate-impl prefilter \
  --order-seed 77 --seed 911 --held-out
```

Do not tune from this result. The 95% one-sided lower bound on both corpora is
the campaign's confirmatory performance result. Record every held-out observation
and final bound in `LOG.md`.

## Known limitations

- **Goodhart on un-corpus'd inputs:** a candidate faster on the pinned corpora
  but subtly wrong elsewhere passes the gate. The corpora are deliberately
  adversarial but not exhaustive. The highest-risk candidate is a cached DFA.
- **Quiet-machine invariant:** the stats assume no concurrent load; numbers are
  not portable across machines. `LOG.md` records the CPU model and online vCPUs.
- **Single-dimension oracle:** regex has one span-set check, so it cannot catch
  distributional drift the way a multi-statistic quality gate can. The
  intentional-semantics-change path is correspondingly weaker — a known
  limitation, not a bug.
