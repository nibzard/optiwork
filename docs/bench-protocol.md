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

## Non-timed paths

The bench has three modes that do **not** emit a fixed record and are never run
in the timed path:

- `--check <golden>` — equivalence gate: loads golden spans and requires exact
  span-set equality for every pair. Prints `PASS impl=… pairs=… spans_checked`
  or `FAIL …`. Exits nonzero on any mismatch.
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
