# Bench Record Protocol

The bench binary (`regexbench/examples/bench.rs`) and the paired runner
(`optikit-paired`) communicate over a single line of stdout: one tab-separated,
versioned record per process. This document is the wire contract.

## Version

```
optiwork-fixed-v1
```

This string is **not** wire-compatible with fenrin's `fenrin-fixed-v1` binaries.
The bump is intentional: the field set and semantics differ (the work unit is
bytes scanned, not names generated). The runner rejects any record whose version
token does not match exactly. A pre-protocol binary cannot participate.

## Record fields

The bench emits, in order, tab-separated:

| field | meaning |
|---|---|
| `version` | `optiwork-fixed-v1` |
| `mode` | measurement path; must equal the runner's `--measure` (currently `scan`) |
| `seed` | base work seed driving the pair-order permutation |
| `count` | corpus bytes scanned per session (== corpus byte total) |
| `sessions` | timed sessions (after one untimed warmup) |
| `warmup_sessions` | untimed warmups, currently `1` |
| `requested` | `count * sessions` |
| `completed` | work actually done; must equal `requested` (no adaptive early-exit) |
| `attempts` | timed `find_all` scans performed (`sessions × corpus pairs`; the warmup scan is excluded) |
| `elapsed_ns` | nanoseconds spent in the timed sessions |
| `items_per_second` | `completed * 1e9 / elapsed_ns` — bytes scanned/sec, the primary metric |
| `output_bytes` | total bytes covered by reported matches (diagnostic) |

The runner validates `mode`, `requested`, `completed == requested`, and that
`count` matches the corpus it intends to scan. Any mismatch fails the block.

## Validation rules

- Exactly one record line per process on stdout. Anything else is malformed.
- `completed` must equal `requested`. The bench performs fixed, non-adaptive
  work; an early-exit (e.g. `findfirst` mode that stops at the first match) would
  under-count and is rejected.
- Exit 0 on success. A nonzero exit invalidates the block: it is logged, never
  replaced, and causes a failing runner exit.
- `--count` must equal the corpus byte total. The bench checks this itself so a
  misconfigured runner cannot produce a silently wrong record.
- `count * sessions` and `blocks * 4` must fit their integer domains. The runner
  rejects oversized designs before allocating a schedule or starting a child.

An invalid block is emitted with `evidence=invalid_design`; it is never labeled
as positive, negative, or calibration evidence. The runner exits nonzero after
recording the failure, and a campaign must classify it as an operational failure
rather than a candidate rejection.

Decision-bearing floating-point fields in `RESULT` (`log_ratio_sd`, ratios, and
log ratios) are rendered with round-trip precision. A campaign therefore compares
the computed confidence bound, not a display-rounded approximation of it.

## Safe argument and process handling

Preferred runner options are repeatable direct arguments:

```sh
optikit-paired \
  --baseline /build/baseline-bench \
  --candidate /build/candidate-bench \
  --baseline-arg --variant --baseline-arg "baseline with spaces" \
  --candidate-arg --variant --candidate-arg "candidate with spaces" \
  --measure scan --count 10000 --sessions 20 --blocks 8 \
  --order-seed 1 --seed 42
```

`--subject-arg` is the A/A equivalent. The legacy `--baseline-args`,
`--candidate-args`, and `--subject-args` whitespace-split blobs remain available
for compatibility, but direct and legacy transports are globally exclusive: any
`--*-arg` option makes every `--*-args` option invalid in that invocation.
Campaigns use only direct arguments so paths and values are preserved exactly.
When all exact argument arrays are empty, `--direct-args` selects the direct
transport explicitly and ensures the subject receives no synthetic legacy
`--subject-args ""` option. Campaigns always pass this selector.

Every observation is bounded by `--timeout-ms` (default 300000) and
`--max-output-bytes` per stream (default 1048576, hard maximum 67108864). Stdout
and stderr are drained while the child runs to avoid pipe deadlocks. On Unix, a
subject runs in its own process group, and a timeout kills that whole group,
reaps the direct child, and joins its pipe readers. Other platforms fall back to
killing the direct child. Output overflow, invalid UTF-8/record shape, a launch
error, or a nonzero exit invalidates the design. Captured diagnostics remain
available in the campaign's raw evidence files.

## Non-timed paths

The bench has three modes that do **not** emit a fixed record and are never run
in the timed path:

- `--check <golden>` — equivalence gate: loads golden spans and requires exact
  span-set equality for every pair. Prints `PASS impl=… pairs=… spans_checked`
  or `FAIL …`. Exit `0` passes, exit `1` is a valid mismatch, and exit `2` is an
  operational/configuration failure, matching the campaign gate contract.
- `--count-of <corpus>` — prints the corpus byte count, so the runner/script can
  set `--count` exactly.
- `--help` — usage.

## Oracle and golden vectors

Golden vectors are produced offline by `examples/gen_golden.rs` (requires the
`oracle` feature, which pulls in the `regex` crate). The oracle is
`regex::bytes::Regex` with a `(?-u)` (ASCII, byte-oriented) prefix, so its
leftmost-first semantics match every impl. The `regex` crate is **never** linked
into the timed bench binary — it is dev/oracle-only, gated behind the cargo
feature. Rebuild golden vectors only when the corpus changes:

```sh
cargo run -p regexbench --features oracle --example gen_golden -- corpora
```

## PGO manifest protocol

`scripts/build-pgo.sh` writes a separate manifest with header
`format=optiwork-pgo-v1`, recording source revision/state, rustc/cargo/host,
llvm-profdata version, the training `(impl, corpus)` matrix, seed, and session
count. Like the bench record, this version string is distinct and not compatible
with fenrin's PGO manifest.
