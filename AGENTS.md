# AGENTS.md — working instructions for optiwork

optiwork is a reusable optimization-measurement harness proved on a regex-engine
demo. Before changing anything here, internalize the discipline below — most of it
is **non-negotiable** because violating it silently corrupts the headline results.

## Read first

1. [`README.md`](README.md) — what the project is and the headline result.
2. [`docs/optimization-loop.md`](docs/optimization-loop.md) — the measurement
   lifecycle: immutable artifacts, A/A calibration, cumulative promotion, the
   frozen keep/reject rule, held-out confirmation.
3. [`docs/bench-protocol.md`](docs/bench-protocol.md) — the `optiwork-fixed-v1`
   record wire format and validation rules.
4. [`docs/sota-2026.md`](docs/sota-2026.md) — regex-engine SOTA leaderboard,
   the techniques that drive it, and what optiwork should be aiming for.
5. [`LOG.md`](LOG.md) — the historical campaign log (format reference; **new runs
   do not append to it** — they write self-contained records under `target/runs/`).

## Non-negotiable measurement discipline

These exist so a speed difference between matchers is provably **performance**, not
noise or a semantics change.

- **Equivalence gate before timing.** Every matcher must produce byte-identical
  match spans vs the `regex`-crate oracle (leftmost-first, greedy). `bench --check`
  against the golden vectors is fail-closed; a candidate never gets timed until it
  passes. If you add a matcher, it must pass the gate on **both** corpora.
- **The oracle is oracle-only.** The `regex` crate is gated behind the `oracle`
  cargo feature and must **never** be linked into the timed candidate binary. It is
  used for golden-vector generation and (opt-in) the same-machine SOTA baseline —
  not for any candidate under optimization. Keep that boundary intact.
- **`regexbench` lib is `#![forbid(unsafe_code)]`.** `optikit` stays
  dependency-free (no I/O, no subprocess). Do not weaken either invariant.
- **Fixed-work contract.** `count` = corpus bytes, `requested = count*sessions`,
  and the bench **must** report `completed == requested` (no adaptive early-exit).
  The work is pinned; the time is what varies.
- **Frozen two-corpus keep/reject rule.** A candidate promotes **only if** all
  gates pass **and** the 95% one-sided lower-bound ratio beats 1.0 on **both**
  `main` and `pathological`. Do not relax this to win on one corpus — the
  `thompson` rejection in the headline is the point of the demo.
- **Quiet-machine invariant.** The statistics assume no concurrent load; numbers are
  not portable across machines. Record CPU model + online vCPUs in any new run. Do
  not present cross-machine numbers as comparable.

## Workflow when iterating

- Reproduce before changing: `bash scripts/run-campaign.sh target/runs/<name>`
  produces the gates + campaign in a fresh run directory.
- Gates run in order, fail-closed: `cargo fmt --check` → `cargo test -p regexbench
  --all-targets` → `cargo clippy --all-targets -- -D warnings` → `bench --check`.
  Keep them green. Run `cargo fmt --all` before committing (fmt drift is the
  common failure).
- New artifacts are immutable: a run directory's `spec.json`, `state.json`,
  `events.jsonl`, `provenance.json`, `raw/`, and `report.md` are the record. Don't
  edit them in place.
- Held-out confirmation uses **fresh seeds** against the **original** baseline,
  exactly once per campaign.

## Output conventions

- The user bans certain overused AI metaphors from all written output. Before
  finalizing any text, rephrase around "spine", "load-bearing", "robust", and
  "production-ready" (metaphorical sense; literal use is fine).
- Report results faithfully: if a gate fails, say so with the output; if a step was
  skipped, say that; don't present an unverified number as measured.
