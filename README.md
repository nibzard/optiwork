# optiwork

A reusable optimization-measurement harness, proved on a regex-engine demo.

Most "is it faster?" work is decided by single-run timers that conflate real
gains with noise, build differences, and luck. `optiwork` extracts the
measurement discipline that makes optimization decisions trustworthy:

- a **fixed-work** contract (the work is pinned, not the time);
- a **versioned subprocess protocol** so the runner cannot tell candidates apart;
- **paired A/B + A/A** scheduling with a Student-t 95% one-sided lower bound;
- a **fail-closed equivalence gate** (correctness before timing);
- a **frozen, pre-registered keep/reject rule**, written down before the result
  is seen, and a **held-out confirmation** reserved for fresh data.

The harness is generic. The demo subject is a small regex engine with three
matchers — a naive backtracker, a Thompson-NFA Pike VM, and a literal-prefix
prefilter — that must all produce byte-identical match spans. A speed difference
between them is therefore provably performance, never a semantics change.

## The headline result

Running `scripts/run-campaign.sh` drives two candidates through the harness
against a naive baseline, each on two corpora (`main`: benign, `pathological`:
catastrophic-backtracking). The frozen rule promotes a candidate only if its 95%
lower bound beats 1.0 on **both** corpora.

| candidate | main | pathological | decision |
|---|---|---|---|
| `thompson` (Pike VM) | **−41.6%** (regression) | **+128,413×** | rejected |
| `prefilter` (memchr prefix) | **+127%** (lower95 = 2.25) | **+134,341×** | **promoted** |

The interesting part is the **rejection**. A Pike VM is ~128,000× faster on
catastrophic input — an obviously "better" engine — but its per-byte thread-list
overhead is slower than tight backtracking on benign input, so it regresses on
`main`. The two-corpus rule refuses to promote a specialization that one corpus
penalizes. The prefilter then wins on both: SIMD-skipping to literal hits on the
large haystack, and inheriting the linear Pike VM on pathological patterns. Both
decisions were reproduced by held-out runs with fresh seeds.

Full numbers and the preregistered `PLAN`/`RESULT` records are in [`LOG.md`](LOG.md).

## Workspace

```
crates/
  optikit/          dependency-free lib: FixedRecord, schedule RNG, statistics
  optikit-paired/   generic A/B + A/A runner (subprocess + versioned record)
  optikit-campaign/ scripted campaign driver (gate → paired → keep/reject → LOG)
  regexbench/       the demo subject (lib + bench/gen_golden examples)
configs/, corpora/, scripts/, docs/
```

`optikit` has no dependencies and no I/O. The runners depend only on `optikit` +
std. `regexbench` adds `memchr` (timed path) and `regex` (oracle-only, behind the
`oracle` cargo feature, never linked into the timed binary).

## Quick start

```sh
# correctness gates + the full campaign (appends to LOG.md)
bash scripts/run-campaign.sh

# regenerate corpora + golden vectors (only when the corpus changes)
cargo run -p regexbench --features oracle --example gen_golden -- corpora

# one standalone timed observation
cargo build --release -p regexbench --example bench
./target/release/examples/bench --measure scan --impl prefilter \
  --corpus corpora/main.bin --seed 42 --sessions 30 --count 8377

# profile-guided build of the bench binary
scripts/build-pgo.sh myrun
```

## The matchers (`--impl`)

All three implement `find_all(re, input, &mut out)` with identical leftmost-first
greedy semantics, verified against the `regex`-crate oracle.

- **`naive`** — recursive backtracking. Correct, and fast on benign input, but
  exponential on catastrophic-backtracking patterns. It exists to be the slow
  baseline.
- **`thompson`** — Thompson construction compiled to an instruction program,
  simulated by a Pike VM. Linear time, no backtracking, with submatch capture
  fused into the simulation.
- **`prefilter`** — scans for a required leading literal with SIMD `memchr`, then
  verifies each hit with an anchored Pike-VM run. Patterns with no selective
  literal prefix fall through to the full Thompson scan.

## Reproducibility

Numbers are **not portable across machines** — the statistics assume a quiet
machine. This campaign was recorded on a 12th Gen Intel Core i9-12900HK (4 online
vCPUs). Corpus provenance (SHA-256 of the generated artifacts):

```
775ad595ebb5710a7bb8e6cb7c46497ebeaa8f4bcb630de23741c3066cc71d21  corpora/main.bin
a775fc7488cc2fc469c5865722a8f7d1267d92e5abf0d434643a09c1682a53b0  corpora/pathological.bin
502c11241a2b86ff29831b4eb3688f53347b41a00ca866f4c5530616ee80e1df  corpora/main.golden
23db17ca251899479a478eb42f776fefdf906194042f1133b3bffcde9ef0ce1e  corpora/pathological.golden
```

Oracle: `regex` 1.13.1, byte-oriented (`(?-u)`), leftmost-first.

## Documentation

- [`docs/optimization-loop.md`](docs/optimization-loop.md) — the full measurement
  protocol: fixed work, A/A calibration, A/B on both corpora, the frozen
  keep/reject rule, held-out confirmation, known limitations.
- [`docs/bench-protocol.md`](docs/bench-protocol.md) — the `optiwork-fixed-v1`
  record wire format and validation rules.

## Origin

The measurement machinery is extracted from
[Fenrin](https://github.com/nibzard/fenrin)'s name-generation optimization
campaign, generalized off of its subject. Fenrin is untouched. The subprocess +
versioned-record protocol, the paired statistics, the PGO script, and the
optimization-loop document all port from Fenrin; the subject, corpus, oracle, and
candidate ladder are new.
