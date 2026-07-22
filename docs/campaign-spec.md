# Campaign Specification and Run Record

`optikit-campaign` consumes one versioned JSON document and creates one new run
directory:

```sh
optikit-campaign --spec <campaign.json> --run-dir <new-directory>
```

The directory must not exist. Campaigns are intentionally not resumed or
appended: if a run is interrupted, preserve its evidence and start a new run
directory.

## Version 1 schema

The accepted top-level version is `optiwork-campaign-v1`.

```json
{
  "version": "optiwork-campaign-v1",
  "id": "example",
  "paired": "/build/optikit-paired",
  "measure": "scan",
  "environment": {
    "RAYON_NUM_THREADS": "1"
  },
  "limits": {
    "gate_timeout_ms": 300000,
    "subject_timeout_ms": 300000,
    "paired_timeout_ms": 43200000,
    "max_output_bytes": 16777216
  },
  "max_candidates": 2,
  "baseline": {
    "id": "original",
    "binary": "/build/original-bench",
    "args": ["--variant", "original"]
  },
  "candidates": [
    {
      "id": "candidate-1",
      "binary": "/build/candidate-1-bench",
      "args": ["--variant", "candidate-1"],
      "hypothesis": "Remove one avoidable allocation from the hot path."
    }
  ],
  "workloads": [
    {
      "id": "representative",
      "args": ["--input", "/data/workload.bin"],
      "gate_args": ["--check", "/data/expected.bin"],
      "artifacts": ["/data/workload.bin", "/data/expected.bin"],
      "count": 10000,
      "sessions": 20,
      "calibration_blocks": 8,
      "min_blocks": 8,
      "max_blocks": 32
    }
  ],
  "calibration": {
    "order_seed": 731,
    "seeds": [101, 202, 303],
    "target_speedup_percent": 3.0,
    "max_abs_mean_log_ratio": 0.03
  },
  "exploration": {
    "order_seed": 1,
    "seeds": [42]
  },
  "confirmation": {
    "order_seed": 77,
    "seeds": [911]
  },
  "decision": {
    "min_lower_bound_ratio": 1.0
  }
}
```

Artifact `args` precede workload arguments. A gate command is:

```text
<artifact.binary> <artifact.args...> <workload.args...> <workload.gate_args...>
  --optiwork-gate-artifact-id <artifact.id>
  --optiwork-gate-workload-id <workload.id>
```

`workload.args` is deliberately common to correctness and timing;
`workload.gate_args` contains only gate-specific additions. This makes it
impossible to accidentally validate one corpus/configuration and time another.

The gate exit contract is explicit: `0` means equivalent, `1` means a valid
equivalence mismatch, and any other status (including a signal or launch error)
is an operational failure. Exit `0` is accepted only with exactly one
`optiwork-gate-v1` stdout record echoing the supplied artifact/workload IDs,
`status=equivalent`, and a positive `checked_units` count. Only a valid exit `1`
becomes the candidate outcome `gate_failed`; launch, protocol, and other
operational failures abort the campaign.

A timed observation receives the paired runner's protocol arguments (`--measure`,
`--seed`, `--count`, and `--sessions`) followed by artifact arguments and then
`workload.args`. Arguments are passed directly, not split or interpreted by a
shell.

`workload.artifacts` declares non-executable inputs whose bytes belong to the
experiment. It does not add command arguments; list every corpus, fixture,
configuration, model, or golden file that should be fingerprinted. Paths in the
checked-in showcase are relative to the working directory from which the
campaign is launched.

`decision.min_lower_bound_ratio` must be at least `1.0`, and promotion uses a
strict `>` comparison. Higher values express a preregistered minimum convincing
speedup; values below `1.0` are rejected because they would allow a measured
regression to be labeled as an optimization.

`calibration.max_abs_mean_log_ratio` is the preregistered maximum tolerated A/A
label bias. Calibration aborts if the same artifact's absolute mean log-ratio
exceeds it. The driver also recomputes the power recommendation with the shared
statistics kernel. If the recommendation exceeds a workload's `max_blocks`, the
frozen cap is honored but the run record explicitly marks the design as under
the requested target power.

Child processes do not inherit the campaign's ambient environment. The driver
clears it and installs exactly the string keys and values in `environment` for
both gates and the paired runner; benchmark subjects inherit the same frozen map
from the paired runner. Declare thread-count, allocator, locale, and subject
configuration variables here when they matter. Executable paths are resolved
before launch, so `PATH` is not required by the harness itself.

`limits` bounds each process layer. Gate, individual subject, and whole
paired-runner timeouts are milliseconds; `max_output_bytes` applies separately
to stdout and stderr at both capture layers and cannot exceed 64 MiB. The paired
timeout must exceed the worst-case frozen block schedule at the declared subject
timeout, ensuring the paired runner can clean up its own child before the outer
bound. Exceeding any limit is an operational failure with bounded partial
diagnostics preserved under `raw/`.

## Validation

Validation finishes before measurement and rejects:

- an unknown version, unknown CLI option, missing path, or existing run directory;
- empty or unsafe IDs, duplicate artifact/workload IDs, or too many candidates;
- empty workloads or candidates;
- zero counts, sessions, block limits, or seed lists;
- a block range where `min_blocks > max_blocks`;
- a non-finite/non-positive target effect or A/A bias limit, or a decision
  threshold below `1.0`;
- zero/excessive process limits or invalid environment keys/values;
- reused exploration/confirmation seeds; or
- reused phase order seeds.

The baseline, candidates, paired runner, and declared workload artifacts must be
regular readable files. Executables must also be launchable. Validation errors
are operational failures, not candidate rejections.

## State transitions

The durable phase progression is:

```text
initialized
  → baseline_gate
  → calibration
  → exploration (one transition per frozen candidate)
  → confirmation | confirmation_not_applicable
  → complete
```

An operational error moves the run to `failed`. Candidate-level outcomes are
recorded separately as `promoted`, `not_promoted`, or `gate_failed`; they do not
turn the campaign into a failed run.

Before each candidate comparison the state identifies the current baseline. A
promotion event records `from` and `to`, making it possible to verify that the
next comparison used the promoted artifact. Confirmation names both the final
exploration artifact and the unchanged original baseline. State also records an
`accepted_baseline`: it remains original until the one-shot confirmation passes,
even though `current_baseline` preserves the provisional exploration ladder.

## Run-directory contract

| path | purpose |
|---|---|
| `spec.json` | exact input spec bytes copied before execution |
| `provenance.json` | SHA-256 identities of the campaign driver, paired runner, subjects, spec, and declared inputs; frozen child environment; working directory; OS; architecture; and available parallelism |
| `state.json` | atomically replaced snapshot of the latest campaign phase and baseline |
| `events.jsonl` | lossless ordered event stream with sequence, timestamp, campaign, phase, type, and payload |
| `raw/` | unique stdout/stderr files for every gate and paired invocation |
| `report.md` | human-readable calibration, candidate transitions, decisions, and confirmation summary |

The raw paired stdout retains every `PLAN`, `OBS`, `BLOCK`, `CALIBRATION`, and
`RESULT` line. `events.jsonl` is the machine-readable index and decision trail;
`report.md` is a derived convenience view. Do not treat the report alone as the
scientific record.

All state snapshots are written through a temporary file and renamed, so readers
observe either the previous complete snapshot or the next one. Event sequence
numbers are monotonically increasing within a run.

## Exit status

Exit zero means the specified campaign completed with valid evidence. It does
not imply that any candidate promoted or that confirmation was positive. A
nonzero status means the campaign could not produce valid evidence; inspect
`state.json`, the final event, and the corresponding raw output.
