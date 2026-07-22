# Benchmark-Guided Optimization Loop

`optiwork` treats optimization as a campaign, not a sequence of unrelated timer
runs. One immutable JSON specification fixes the artifacts, workloads, candidate
order, noise calibration, decision rule, and confirmation design before the
first candidate result is observed.

The regex engine is only the showcase. The campaign runner knows nothing about
regexes, implementation names, corpora, or golden files; it launches artifact
binaries with argument arrays supplied by the specification.

## The lifecycle

The enforced order is:

```text
validate and fingerprint
        ↓
gate original baseline
        ↓
A/A calibration on the original baseline
        ↓
candidate 1 gate → compare with current baseline → promote or retain
        ↓
candidate 2 gate → compare with current baseline → promote or retain
        ↓
exactly one final-best vs original-baseline confirmation (if anything promoted)
        ↓
complete
```

Candidate order is frozen. A promoted candidate becomes the baseline for the
next candidate; a rejected or gate-failing candidate does not. Exploration never
uses confirmation seeds, and confirmation never changes a promotion decision.

## 1. Freeze artifacts and design

Build the original baseline and every candidate before starting the campaign.
Give each one its own path and keep those files unchanged for the run. In a real
optimization project these are normally builds from separate revisions or
worktrees. The regex showcase uses distinct snapshot paths for a single
runtime-selectable demo executable.

The spec must also freeze:

- the ordered candidate ladder and a hypothesis for each candidate;
- every workload's gate arguments, timed arguments, fixed count, and sessions;
- all input artifacts that must be fingerprinted;
- the exact child environment and gate/paired process limits;
- the A/A seeds, order seed, target effect, and allowed block-count range;
- exploration and confirmation seeds and distinct order seeds; and
- the lower-confidence-bound threshold and maximum candidate count.

Run directories are append-once. The driver refuses an existing directory, copies
the exact spec bytes into the new directory, and records SHA-256 fingerprints of
the runner, binaries, spec, and declared workload artifacts. Changing an input
means starting a new campaign.

## 2. Gate correctness before timing

The campaign first gates the original baseline on every workload. Failure here
means the experiment is malformed and aborts the campaign.

Each candidate is then gated on every workload before any timing for that
candidate. A candidate gate failure is a valid negative candidate outcome: it is
recorded, no paired process is launched for that candidate, the current baseline
is retained, and the campaign proceeds to the next preregistered candidate.

Gate status is a small protocol of its own: exit `0` passes, exit `1` reports a
valid equivalence mismatch, and every other exit status or signal is operational.
This prevents a missing fixture, crashed gate, or invalid invocation from being
reported as a scientifically rejected candidate.

The timed workload arguments are also passed to the gate, followed by only its
gate-specific additions. A successful gate must emit one versioned
`optiwork-gate-v1` record that echoes the artifact and workload identities and a
positive checked-work count. Exit zero without that evidence is operationally
invalid, so a binary cannot pass merely by ignoring an unknown gate option.

For the showcase the gate compares exact match spans with committed golden
vectors:

```sh
bench --impl <artifact> --check <golden> --corpus <corpus>
```

Other subjects can use any deterministic, fail-closed gate expressed by their
artifact's command-line interface.

## 3. Calibrate A/A before exploration

The same original-baseline artifact runs under both labels using a paired
ABBA/BAAB schedule. A/A estimates environmental noise; it is never evidence that
an optimization works.

For each workload, `optikit-paired` reports a recommended block count for the
predeclared target speedup. The campaign applies the frozen rule

```text
chosen_blocks = clamp(recommended_blocks, min_blocks, max_blocks)
```

before observing any candidate. An invalid A/A design or failed observation
aborts the campaign. It cannot silently become positive calibration evidence.
The campaign also rejects an A/A mean log-ratio outside the frozen bias limit,
validates the paired plan echo, and independently recomputes the recommended
block count with the shared statistics kernel. If `max_blocks` caps the
recommendation, state and report mark that workload as under the requested
target power; the confidence-bound decision remains conservative, but a likely
false negative is no longer hidden.

## 4. Explore with cumulative promotion

Each valid candidate is compared with the current promoted baseline on every
workload. An ABBA/BAAB block contains two observations per label; analysis uses
the paired block log-throughput ratios. The runner reports the geometric speedup
and a Student-t 95% one-sided lower confidence bound.

The generic frozen rule is:

```text
promote iff every workload's lower_95_one_sided_ratio
           is greater than decision.min_lower_bound_ratio
```

All complete, valid preregistered blocks remain in the analysis, including
unusually fast or slow ones. The campaign does not add blocks, retry a statistical
loss, drop outliers, reorder candidates, or change thresholds after results are
known.

These per-candidate results are exploratory. Testing multiple candidates is a
selection process, so none of their individual confidence bounds is presented as
the campaign's final confirmatory claim.

## 5. Confirm once on fresh data

If at least one candidate was promoted, the final promoted artifact is compared
with the original baseline exactly once, on every workload, using the frozen
confirmation order seed and work seeds. The confirmation uses the already chosen
block counts; it cannot tune, retry, or promote anything.

If nothing was promoted, the campaign records that confirmation was not
applicable. A valid confirmation that misses the threshold is still a completed
campaign with a negative confirmatory result. The exploratory winner remains in
the ladder history, but the accepted baseline stays original unless confirmation
passes; an inconclusive candidate is never presented as deployable.

## Outcome semantics

The process exit status reports whether the campaign machinery remained valid,
not whether an optimization won:

| outcome | meaning | campaign action | exit |
|---|---|---|---|
| `promoted` | every workload cleared the frozen bound | update current baseline | 0 |
| `not_promoted` | valid evidence missed at least one bound | retain baseline | 0 |
| `gate_failed` | candidate was not equivalent | skip its timing, retain baseline | 0 |
| negative confirmation | valid held-out result missed a bound | record honestly | 0 |
| `operational_failure` | launch, timeout, malformed output, invalid design, or I/O failure | abort; do not classify candidate performance | nonzero |

This distinction prevents infrastructure errors from masquerading as scientific
rejections and prevents ordinary statistical losses from looking like broken
automation.

## Fixed-work subprocess contract

Each observation scans exactly `count × sessions` requested units after untimed
warmup. The subject emits one `optiwork-fixed-v1` record. The paired runner checks
the protocol version, mode, requested work, completed work, count, and finite
positive throughput. It launches binaries with direct argument vectors, so paths
and values containing spaces are not reparsed by a shell.

Every child process has a timeout and bounded captured output. A timeout,
nonzero exit, output overflow, malformed record, or work mismatch invalidates the
design and causes a failing runner exit. Invalid blocks are logged and never
replaced.

The campaign applies a separate frozen timeout and per-stream output cap to each
gate and whole paired-runner invocation. It also clears the ambient environment
and supplies only the map recorded in the campaign spec. This prevents an
unrecorded shell variable or a noisy/hung wrapper from silently changing or
stalling the campaign.

See [bench-protocol.md](bench-protocol.md) for the wire record and
[campaign-spec.md](campaign-spec.md) for the orchestrator contract.

## Running the showcase

From the repository root:

```sh
bash scripts/run-campaign.sh target/runs/regex-demo-local
```

The script runs formatting, tests, and Clippy; builds the three immutable demo
artifact snapshots; and invokes:

```sh
target/release/optikit-campaign \
  --spec campaigns/regex-demo.json \
  --run-dir target/runs/regex-demo-local
```

An atomic showcase lock is held from build through campaign completion. A second
showcase run therefore cannot overwrite the shared snapshot paths or introduce
concurrent benchmark load; use a separate project/spec for intentional parallel
experiments.

Corpus and golden-vector files are committed, declared inputs. The campaign
script deliberately does not regenerate them. Regeneration changes the
experiment and should be followed by review and a new spec/run.

## Known limitations

- Confidence intervals describe the frozen workloads, not uncaptured production
  distributions. Improve workload coverage instead of weakening the gate.
- The paired statistics assume stable machine conditions and no concurrent
  benchmark series. Numbers are not portable across hosts.
- SHA-256 proves which bytes participated; it does not prove how a binary was
  built. Preserve build manifests or revision metadata alongside serious runs.
- A single threshold across workloads is intentionally strict. Projects needing
  weighted tradeoffs should define and version a new decision policy rather than
  interpreting this one informally.
