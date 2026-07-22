// ABOUTME: End-to-end tests for the whole-campaign JSON driver.
// ABOUTME: Fake bench and paired executables make ladder transitions and failures observable.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const FAKE_BENCH: &str = r#"#!/bin/sh
if [ "${AMBIENT_ONLY+x}" = "x" ]; then
  printf 'ambient environment leaked into gate\n' >&2
  exit 88
fi
if [ -n "${FAKE_BENCH_LOG:-}" ]; then
  printf '%s\n' "$*" >> "$FAKE_BENCH_LOG"
fi

artifact=
workload=
gate_artifact_id=
gate_workload_id=
gate_requested=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --artifact)
      artifact=${2:-}
      shift 2
      ;;
    --workload)
      workload=${2:-}
      shift 2
      ;;
    --gate)
      gate_requested=1
      shift
      ;;
    --optiwork-gate-artifact-id)
      gate_artifact_id=${2:-}
      shift 2
      ;;
    --optiwork-gate-workload-id)
      gate_workload_id=${2:-}
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done

if [ "$artifact" = "bad" ]; then
  printf 'FAIL artifact=%s mismatched_pairs=1/1\n' "$artifact" >&2
  exit 1
fi
if [ "$artifact" = "broken" ]; then
  printf 'could not load fixture for artifact=%s\n' "$artifact" >&2
  exit 2
fi
if [ "$artifact" = "hang" ]; then
  printf 'partial gate stdout for artifact=%s\n' "$artifact"
  printf 'partial gate stderr for artifact=%s\n' "$artifact" >&2
  /bin/sleep 30 &
  wait
fi
if [ "$artifact" = "noisy" ]; then
  i=0
  while [ "$i" -lt 10000 ]; do
    printf 'noisy gate output artifact=%s line=%s 012345678901234567890123456789\n' "$artifact" "$i"
    i=$((i + 1))
  done
fi
if [ "$artifact" = "no-record" ]; then
  exit 0
fi
if [ "$gate_requested" -ne 1 ] || [ "$workload" != "main" ] || \
   [ "$gate_artifact_id" != "$artifact" ] || [ "$gate_workload_id" != "$workload" ]; then
  printf 'invalid gate handshake artifact=%s workload=%s gate_artifact=%s gate_workload=%s\n' \
    "$artifact" "$workload" "$gate_artifact_id" "$gate_workload_id" >&2
  exit 2
fi
printf 'optiwork-gate-v1\tstatus=equivalent\tartifact_id=%s\tworkload_id=%s\tchecked_units=1\n' \
  "$gate_artifact_id" "$gate_workload_id"
"#;

// The campaign must forward each artifact/workload token through repeated
// --subject-arg, --baseline-arg, and --candidate-arg options. This fake decodes
// those streams and records the resulting artifacts so tests can prove that a
// promotion changes the next comparison's baseline.
const FAKE_PAIRED: &str = r#"#!/bin/sh
if [ "${AMBIENT_ONLY+x}" = "x" ]; then
  printf 'ambient environment leaked into paired runner\n' >&2
  exit 88
fi
original=$*
experiment=AB
held_out=0
baseline_artifact=
candidate_artifact=
subject_artifact=
workload=
baseline_needs_artifact=0
candidate_needs_artifact=0
subject_needs_artifact=0
baseline_needs_workload=0
candidate_needs_workload=0
subject_needs_workload=0
blocks=2
target_speedup=3.0
measure=scan
count=0
sessions=0
order_seed=0
seeds=
timeout_ms=0
max_output_bytes=0
direct_args=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --aa)
      experiment=AA
      shift 2
      ;;
    --baseline|--candidate)
      shift 2
      ;;
    --measure) measure=${2:-}; shift 2;;
    --count) count=${2:-0}; shift 2;;
    --sessions) sessions=${2:-0}; shift 2;;
    --order-seed) order_seed=${2:-0}; shift 2;;
    --seed)
      if [ -z "$seeds" ]; then seeds=${2:-}; else seeds="$seeds,${2:-}"; fi
      shift 2
      ;;
    --timeout-ms) timeout_ms=${2:-0}; shift 2;;
    --max-output-bytes) max_output_bytes=${2:-0}; shift 2;;
    --direct-args) direct_args=1; shift;;
    --blocks)
      blocks=${2:-2}
      shift 2
      ;;
    --target-speedup)
      target_speedup=${2:-3.0}
      shift 2
      ;;
    --held-out)
      held_out=1
      shift
      ;;
    --baseline-arg)
      value=${2:-}
      if [ "$baseline_needs_artifact" -eq 1 ]; then
        baseline_artifact=$value
        baseline_needs_artifact=0
      elif [ "$baseline_needs_workload" -eq 1 ]; then
        workload=$value
        baseline_needs_workload=0
      elif [ "$value" = "--artifact" ]; then
        baseline_needs_artifact=1
      elif [ "$value" = "--workload" ]; then
        baseline_needs_workload=1
      fi
      shift 2
      ;;
    --candidate-arg)
      value=${2:-}
      if [ "$candidate_needs_artifact" -eq 1 ]; then
        candidate_artifact=$value
        candidate_needs_artifact=0
      elif [ "$candidate_needs_workload" -eq 1 ]; then
        workload=$value
        candidate_needs_workload=0
      elif [ "$value" = "--artifact" ]; then
        candidate_needs_artifact=1
      elif [ "$value" = "--workload" ]; then
        candidate_needs_workload=1
      fi
      shift 2
      ;;
    --subject-arg)
      value=${2:-}
      if [ "$subject_needs_artifact" -eq 1 ]; then
        subject_artifact=$value
        subject_needs_artifact=0
      elif [ "$subject_needs_workload" -eq 1 ]; then
        workload=$value
        subject_needs_workload=0
      elif [ "$value" = "--artifact" ]; then
        subject_needs_artifact=1
      elif [ "$value" = "--workload" ]; then
        subject_needs_workload=1
      fi
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done

if [ "$experiment" = "AA" ]; then
  phase=calibration
  result_scope=calibration
  baseline_artifact=$subject_artifact
elif [ "$held_out" -eq 1 ]; then
  phase=confirmation
  result_scope=held_out_confirmation
else
  phase=exploration
  result_scope=exploratory_per_candidate
fi

if [ -n "${FAKE_PAIRED_LOG:-}" ]; then
  printf 'phase=%s baseline=%s candidate=%s workload=%s held_out=%s argv=%s\n' \
    "$phase" "$baseline_artifact" "$candidate_artifact" "$workload" "$held_out" "$original" \
    >> "$FAKE_PAIRED_LOG"
fi

if [ -n "${FAKE_PAIRED_HANG:-}" ]; then
  printf 'partial paired stdout phase=%s\n' "$phase"
  printf 'partial paired stderr phase=%s\n' "$phase" >&2
  /bin/sleep 30 &
  wait
fi

if [ -n "${FAKE_PAIRED_FAIL_ARTIFACT:-}" ] && \
   [ "$candidate_artifact" = "$FAKE_PAIRED_FAIL_ARTIFACT" ]; then
  printf 'intentional paired failure for artifact=%s\n' "$candidate_artifact" >&2
  exit 9
fi

case "$order_seed" in
  731) schedule=ABBA,ABBA;;
  11) schedule=BAAB,BAAB;;
  77) schedule=BAAB,ABBA;;
  *) schedule=ABBA,BAAB;;
esac
requested=$((count * sessions))
if [ "$direct_args" -ne 1 ]; then
  printf 'campaign did not select direct argument transport\n' >&2
  exit 9
fi

if [ "$experiment" = "AA" ]; then
  printf 'PLAN protocol=optiwork-paired-v1 experiment=AA scope=calibration mode=%s argument_transport=direct baseline_argv=[] candidate_argv=[] count=%s sessions=%s requested=%s blocks=%s order_source=random:%s schedule=%s seeds=%s timeout_ms=%s max_output_bytes_per_stream=%s\n' \
    "$measure" "$count" "$sessions" "$requested" "$blocks" "$order_seed" "$schedule" "$seeds" "$timeout_ms" "$max_output_bytes"
  printf 'RESULT protocol=optiwork-paired-v1 experiment=AA scope=calibration mode=scan valid_blocks=%s planned_blocks=%s invalid_blocks=0 mean_log_ratio=0.000000000 log_ratio_sd=0.010000000 speedup_ratio=1.000000 speedup_percent=0.000 lower_95_one_sided_ratio=0.990000 lower_95_one_sided_percent=-1.000 evidence=calibration_only\n' "$blocks" "$blocks"
  printf 'CALIBRATION protocol=optiwork-paired-v1 target_speedup_percent=%s approximate_blocks_for_80_percent_power=2\n' "$target_speedup"
  exit 0
fi

if [ "$held_out" -eq 1 ]; then
  lower=1.100000
  evidence=candidate_faster
elif [ "$candidate_artifact" = "c2" ]; then
  lower=0.900000
  evidence=screen_inconclusive
else
  lower=1.200000
  evidence=screen_positive
fi
printf 'PLAN protocol=optiwork-paired-v1 experiment=AB scope=%s mode=%s argument_transport=direct baseline_argv=[] candidate_argv=[] count=%s sessions=%s requested=%s blocks=%s order_source=random:%s schedule=%s seeds=%s timeout_ms=%s max_output_bytes_per_stream=%s\n' \
  "$result_scope" "$measure" "$count" "$sessions" "$requested" "$blocks" "$order_seed" "$schedule" "$seeds" "$timeout_ms" "$max_output_bytes"
printf 'RESULT protocol=optiwork-paired-v1 experiment=AB scope=%s mode=scan valid_blocks=%s planned_blocks=%s invalid_blocks=0 mean_log_ratio=0.095310180 log_ratio_sd=0.010000000 speedup_ratio=1.100000 speedup_percent=10.000 lower_95_one_sided_ratio=%s lower_95_one_sided_percent=10.000 evidence=%s\n' \
  "$result_scope" "$blocks" "$blocks" "$lower" "$evidence"
"#;

static NEXT_CASE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    bench_log: PathBuf,
    paired_log: PathBuf,
    spec_path: PathBuf,
    run_dir: PathBuf,
    spec: Value,
}

impl Fixture {
    fn new(test_name: &str, candidates: &[&str]) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "campaign-driver-{test_name}-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();

        let bench = root.join("fake-bench.sh");
        let paired = root.join("fake-paired.sh");
        write_script(&bench, FAKE_BENCH);
        write_script(&paired, FAKE_PAIRED);

        let candidate_specs = candidates
            .iter()
            .map(|id| {
                json!({
                    "id": id,
                    "binary": bench,
                    "args": ["--artifact", id],
                    "hypothesis": format!("hypothesis for {id}"),
                })
            })
            .collect::<Vec<_>>();
        let spec = json!({
            "version": "optiwork-campaign-v1",
            "id": format!("campaign-{test_name}"),
            "paired": paired,
            "measure": "scan",
            "environment": {
                "FAKE_BENCH_LOG": root.join("bench-invocations.log"),
                "FAKE_PAIRED_LOG": root.join("paired-invocations.log"),
            },
            "limits": {
                "gate_timeout_ms": 5000,
                "subject_timeout_ms": 1,
                "paired_timeout_ms": 5000,
                "max_output_bytes": 4096,
            },
            "max_candidates": candidates.len(),
            "baseline": {
                "id": "base",
                "binary": bench,
                "args": ["--artifact", "base"],
            },
            "candidates": candidate_specs,
            "workloads": [{
                "id": "main",
                "args": ["--workload", "main"],
                "gate_args": ["--gate"],
                "artifacts": [],
                "count": 10,
                "sessions": 2,
                "calibration_blocks": 2,
                "min_blocks": 2,
                "max_blocks": 4,
            }],
            "calibration": {
                "order_seed": 731,
                "seeds": [1],
                "target_speedup_percent": 3.0,
                "max_abs_mean_log_ratio": 0.03,
            },
            "exploration": {
                "order_seed": 11,
                "seeds": [42],
            },
            "confirmation": {
                "order_seed": 77,
                "seeds": [911],
            },
            "decision": {
                "min_lower_bound_ratio": 1.0,
            },
        });
        let spec_path = root.join("spec-input.json");
        fs::write(&spec_path, serde_json::to_vec_pretty(&spec).unwrap()).unwrap();

        Self {
            bench_log: root.join("bench-invocations.log"),
            paired_log: root.join("paired-invocations.log"),
            spec_path,
            run_dir: root.join("run"),
            root,
            spec,
        }
    }

    fn rewrite_spec(&self) {
        fs::write(
            &self.spec_path,
            serde_json::to_vec_pretty(&self.spec).unwrap(),
        )
        .unwrap();
    }

    fn set_child_environment(&mut self, key: &str, value: &str) {
        self.spec["environment"]
            .as_object_mut()
            .unwrap()
            .insert(key.to_owned(), json!(value));
        self.rewrite_spec();
    }

    fn set_limit(&mut self, key: &str, value: u64) {
        self.spec["limits"][key] = json!(value);
        self.rewrite_spec();
    }

    fn run(&self, extra_env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_optikit-campaign"));
        command
            .arg("--spec")
            .arg(&self.spec_path)
            .arg("--run-dir")
            .arg(&self.run_dir);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn paired_lines(&self) -> Vec<String> {
        read_lines_if_present(&self.paired_log)
    }

    fn bench_lines(&self) -> Vec<String> {
        read_lines_if_present(&self.bench_log)
    }

    fn state(&self) -> Value {
        read_json(&self.run_dir.join("state.json"))
    }

    fn events(&self) -> Vec<Value> {
        fs::read_to_string(self.run_dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn raw_bytes(&self, filename_fragment: &str) -> Vec<u8> {
        let path = fs::read_dir(self.run_dir.join("raw"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(filename_fragment)
            })
            .unwrap_or_else(|| panic!("missing raw file containing `{filename_fragment}`"));
        fs::read(path).unwrap()
    }
}

fn write_script(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn read_lines_if_present(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn json_contains_text(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text.contains(needle),
        Value::Array(values) => values.iter().any(|value| json_contains_text(value, needle)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains(needle) || json_contains_text(value, needle)),
        _ => false,
    }
}

fn assert_evidence_bundle(fixture: &Fixture) {
    for name in [
        "spec.json",
        "provenance.json",
        "state.json",
        "events.jsonl",
        "report.md",
    ] {
        let path = fixture.run_dir.join(name);
        assert!(path.is_file(), "missing evidence file: {}", path.display());
    }
    assert_eq!(
        fs::read(&fixture.spec_path).unwrap(),
        fs::read(fixture.run_dir.join("spec.json")).unwrap(),
        "the run must preserve the exact input spec"
    );
    let provenance = read_json(&fixture.run_dir.join("provenance.json"));
    assert_eq!(
        provenance["child_environment"], fixture.spec["environment"],
        "provenance must record the exact frozen child environment"
    );
    assert!(
        provenance["host"].is_object(),
        "host provenance must be separate from the child environment: {provenance}"
    );
    assert!(
        provenance["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["kind"] == "campaign_driver"
                    && entry["id"] == "optikit-campaign"
                    && entry["sha256"]
                        .as_str()
                        .is_some_and(|hash| hash.len() == 64)
            }),
        "campaign driver is missing from provenance: {provenance}"
    );
    let raw = fixture.run_dir.join("raw");
    assert!(raw.is_dir(), "missing raw evidence dir: {}", raw.display());
    assert!(
        fs::read_dir(&raw).unwrap().next().is_some(),
        "raw evidence directory is empty"
    );

    let events = fixture.events();
    assert!(!events.is_empty(), "events.jsonl is empty");
    for (index, event) in events.iter().enumerate() {
        for key in [
            "sequence",
            "timestamp",
            "campaign_id",
            "phase",
            "type",
            "payload",
        ] {
            assert!(
                event.get(key).is_some(),
                "event {index} missing {key}: {event}"
            );
        }
        if index > 0 {
            let previous = events[index - 1]["sequence"].as_u64().unwrap();
            let current = event["sequence"].as_u64().unwrap();
            assert!(current > previous, "event sequences are not increasing");
        }
    }
}

#[test]
fn campaign_promotes_then_rejects_and_confirms_final_against_original() {
    let fixture = Fixture::new("ladder", &["c1", "c2"]);
    let output = fixture.run(&[("AMBIENT_ONLY", "must-not-leak")]);
    assert!(output.status.success(), "{}", output_text(&output));
    assert_evidence_bundle(&fixture);

    let gates = fixture.bench_lines();
    assert!(
        gates.iter().all(|line| {
            line.contains("--workload main")
                && line.contains("--optiwork-gate-artifact-id")
                && line.contains("--optiwork-gate-workload-id main")
        }),
        "gate invocations did not share timed workload args and identity handshake: {gates:#?}"
    );
    let paired = fixture.paired_lines();
    assert!(
        paired.iter().all(|line| {
            line.contains("--direct-args")
                && line.contains("--timeout-ms 1")
                && line.contains("--max-output-bytes 4096")
        }),
        "paired invocations did not freeze exact argv and subject limits: {paired:#?}"
    );
    assert!(
        paired
            .iter()
            .any(|line| line.contains("phase=calibration baseline=base")),
        "paired invocations: {paired:#?}"
    );
    assert!(
        paired.iter().any(|line| {
            line.contains("phase=exploration baseline=base candidate=c1 workload=main")
        }),
        "first exploration did not use the original baseline: {paired:#?}"
    );
    assert!(
        paired.iter().any(|line| {
            line.contains("phase=exploration baseline=c1 candidate=c2 workload=main")
        }),
        "second exploration did not use the promoted candidate: {paired:#?}"
    );
    let confirmations = paired
        .iter()
        .filter(|line| line.contains("phase=confirmation"))
        .collect::<Vec<_>>();
    assert_eq!(confirmations.len(), 1, "paired invocations: {paired:#?}");
    assert!(
        confirmations[0].contains("baseline=base candidate=c1 workload=main"),
        "final confirmation was not final-vs-original: {}",
        confirmations[0]
    );

    let state = fixture.state();
    assert_eq!(state["current_baseline"], "c1", "state: {state}");
    assert_eq!(state["accepted_baseline"], "c1", "state: {state}");
    assert!(json_contains_text(&state, "c1"), "state: {state}");
    assert!(json_contains_text(&state, "complete"), "state: {state}");
    let events = fixture.events();
    assert!(
        events.iter().any(|event| {
            json_contains_text(event, "c1") && json_contains_text(event, "promot")
        }),
        "events do not record c1's promotion: {events:#?}"
    );
    assert!(
        events.iter().any(|event| {
            json_contains_text(event, "c2") && json_contains_text(event, "not_promoted")
        }),
        "events do not record c2's rejection: {events:#?}"
    );
    let report = fs::read_to_string(fixture.run_dir.join("report.md")).unwrap();
    assert!(report.contains("c1"), "report: {report}");
    assert!(report.contains("c2"), "report: {report}");
    assert!(
        report.to_ascii_lowercase().contains("confirmation"),
        "report: {report}"
    );
}

#[test]
fn candidate_gate_failure_skips_its_timing_and_later_candidate_proceeds() {
    let fixture = Fixture::new("gate-continues", &["bad", "c1"]);
    let output = fixture.run(&[]);
    assert!(output.status.success(), "{}", output_text(&output));
    assert_evidence_bundle(&fixture);

    let bench = fixture.bench_lines();
    assert!(
        bench.iter().any(|line| line.contains("--artifact bad")),
        "{bench:#?}"
    );
    assert!(
        bench.iter().any(|line| line.contains("--artifact c1")),
        "{bench:#?}"
    );
    let paired = fixture.paired_lines();
    assert!(
        !paired.iter().any(|line| line.contains("candidate=bad")),
        "a gate-failing candidate reached timing: {paired:#?}"
    );
    assert!(
        paired.iter().any(|line| {
            line.contains("phase=exploration baseline=base candidate=c1 workload=main")
        }),
        "the later candidate did not proceed: {paired:#?}"
    );

    let state = fixture.state();
    assert!(json_contains_text(&state, "c1"), "state: {state}");
    assert!(json_contains_text(&state, "complete"), "state: {state}");
    let events = fixture.events();
    assert!(
        events.iter().any(|event| {
            json_contains_text(event, "bad") && json_contains_text(event, "gate_failed")
        }),
        "events do not record the bad candidate's gate failure: {events:#?}"
    );
    assert!(
        events.iter().any(|event| {
            json_contains_text(event, "c1") && json_contains_text(event, "promot")
        }),
        "events do not record the later promotion: {events:#?}"
    );
}

#[test]
fn scientific_rejections_complete_successfully_without_confirmation() {
    let fixture = Fixture::new("no-promotion", &["c2"]);
    let output = fixture.run(&[]);
    assert!(output.status.success(), "{}", output_text(&output));
    assert_evidence_bundle(&fixture);

    let paired = fixture.paired_lines();
    assert!(
        paired.iter().any(|line| {
            line.contains("phase=exploration baseline=base candidate=c2 workload=main")
        }),
        "candidate was not explored: {paired:#?}"
    );
    assert!(
        !paired
            .iter()
            .any(|line| line.contains("phase=confirmation")),
        "confirmation ran without a promoted candidate: {paired:#?}"
    );
    let state = fixture.state();
    assert!(json_contains_text(&state, "complete"), "state: {state}");
    let events = fixture.events();
    assert!(
        events.iter().any(|event| {
            json_contains_text(event, "c2") && json_contains_text(event, "not_promoted")
        }),
        "events do not record the scientific rejection: {events:#?}"
    );
}

#[test]
fn inconclusive_confirmation_keeps_the_original_as_the_accepted_baseline() {
    let mut fixture = Fixture::new("negative-confirmation", &["c1"]);
    // Exploration clears this threshold (1.20), while held-out confirmation is
    // merely faster than 1.0 (1.10) and must not be accepted.
    fixture.spec["decision"]["min_lower_bound_ratio"] = json!(1.15);
    fixture.rewrite_spec();

    let output = fixture.run(&[]);
    assert!(output.status.success(), "{}", output_text(&output));
    assert_evidence_bundle(&fixture);

    let state = fixture.state();
    assert_eq!(
        state["outcome"], "confirmation_inconclusive",
        "state: {state}"
    );
    assert_eq!(state["current_baseline"], "c1", "state: {state}");
    assert_eq!(state["accepted_baseline"], "base", "state: {state}");
    assert_eq!(
        state["confirmation"]["exploration_winner"], "c1",
        "state: {state}"
    );
    assert_eq!(
        state["confirmation"]["accepted_baseline"], "base",
        "state: {state}"
    );
    assert_eq!(
        state["confirmation"]["workloads"][0]["paired_evidence"], "candidate_faster",
        "state: {state}"
    );
    assert_eq!(
        state["confirmation"]["workloads"][0]["threshold_met"], false,
        "state: {state}"
    );
    let report = fs::read_to_string(fixture.run_dir.join("report.md")).unwrap();
    assert!(
        report.contains("Accepted baseline: `base`"),
        "report: {report}"
    );
    assert!(
        report.contains("accepted baseline: `base`"),
        "report: {report}"
    );
}

#[test]
fn paired_operational_failure_aborts_and_is_not_a_scientific_rejection() {
    let mut fixture = Fixture::new("paired-failure", &["opfail"]);
    fixture.set_child_environment("FAKE_PAIRED_FAIL_ARTIFACT", "opfail");
    let output = fixture.run(&[]);
    assert!(!output.status.success(), "{}", output_text(&output));
    assert_evidence_bundle(&fixture);

    let paired = fixture.paired_lines();
    assert!(
        paired
            .iter()
            .any(|line| line.contains("phase=exploration baseline=base candidate=opfail")),
        "paired invocations: {paired:#?}"
    );
    assert!(
        !paired
            .iter()
            .any(|line| line.contains("phase=confirmation")),
        "confirmation ran after an operational failure: {paired:#?}"
    );
    let state = fixture.state();
    assert!(
        json_contains_text(&state, "fail") || json_contains_text(&state, "error"),
        "state: {state}"
    );
    let events = fixture.events();
    assert!(
        events.iter().any(|event| {
            json_contains_text(event, "fail") || json_contains_text(event, "error")
        }),
        "events do not record the operational failure: {events:#?}"
    );
    assert!(
        !events.iter().any(|event| {
            json_contains_text(event, "opfail") && json_contains_text(event, "reject")
        }),
        "operational failure was mislabeled as a rejection: {events:#?}"
    );
}

#[test]
fn gate_operational_failure_aborts_and_is_not_a_candidate_rejection() {
    let fixture = Fixture::new("gate-operational-failure", &["broken", "c1"]);
    let output = fixture.run(&[]);
    assert!(!output.status.success(), "{}", output_text(&output));
    assert_evidence_bundle(&fixture);

    let paired = fixture.paired_lines();
    assert!(
        !paired.iter().any(|line| line.contains("candidate=broken")),
        "an operationally broken gate reached timing: {paired:#?}"
    );
    assert!(
        !paired.iter().any(|line| line.contains("candidate=c1")),
        "the campaign continued after an operational gate failure: {paired:#?}"
    );
    let state = fixture.state();
    assert!(
        json_contains_text(&state, "operational_failure"),
        "state: {state}"
    );
    let events = fixture.events();
    assert!(
        !events.iter().any(|event| {
            json_contains_text(event, "broken") && json_contains_text(event, "gate_failed")
        }),
        "operational gate failure was mislabeled: {events:#?}"
    );
}

#[test]
fn exit_zero_without_versioned_gate_evidence_aborts() {
    let fixture = Fixture::new("missing-gate-record", &["no-record"]);
    let output = fixture.run(&[]);
    assert!(!output.status.success(), "{}", output_text(&output));
    assert_evidence_bundle(&fixture);

    let state = fixture.state();
    assert_eq!(state["status"], "failed", "state: {state}");
    assert_eq!(state["outcome"], "operational_failure", "state: {state}");
    assert!(
        json_contains_text(&state, "invalid success evidence"),
        "state: {state}"
    );
    assert!(
        !fixture
            .paired_lines()
            .iter()
            .any(|line| line.contains("candidate=no-record")),
        "a candidate without gate evidence reached timing"
    );
}

#[test]
fn hanging_and_noisy_gates_abort_with_bounded_partial_evidence() {
    for (artifact, expected_status, stdout_marker) in [
        ("hang", "timeout", "partial gate stdout"),
        ("noisy", "output_overflow", "noisy gate output"),
    ] {
        let mut fixture = Fixture::new(&format!("gate-{artifact}"), &[artifact]);
        fixture.set_limit("gate_timeout_ms", 1500);
        let started = Instant::now();
        let output = fixture.run(&[]);
        let elapsed = started.elapsed();
        assert!(!output.status.success(), "{}", output_text(&output));
        assert!(
            elapsed < Duration::from_secs(5),
            "{artifact} gate was not bounded: {elapsed:?}"
        );
        assert_evidence_bundle(&fixture);

        let stdout = fixture.raw_bytes(&format!("gate-candidate-{artifact}-main.stdout"));
        let stderr = fixture.raw_bytes(&format!("gate-candidate-{artifact}-main.stderr"));
        assert!(
            String::from_utf8_lossy(&stdout).contains(stdout_marker),
            "missing partial stdout for {artifact}: {}",
            String::from_utf8_lossy(&stdout)
        );
        assert!(
            stdout.len() <= 4096,
            "stdout was not capped: {}",
            stdout.len()
        );
        assert!(
            stderr.len() <= 4096,
            "stderr was not capped: {}",
            stderr.len()
        );
        if artifact == "hang" {
            assert!(
                String::from_utf8_lossy(&stderr).contains("partial gate stderr"),
                "missing partial timeout stderr: {}",
                String::from_utf8_lossy(&stderr)
            );
        }

        let state = fixture.state();
        assert_eq!(state["status"], "failed", "state: {state}");
        assert_eq!(state["outcome"], "operational_failure", "state: {state}");
        assert!(
            json_contains_text(&state, expected_status),
            "state lacks {expected_status}: {state}"
        );
        let events = fixture.events();
        assert!(
            events.iter().any(|event| {
                event["type"] == "command_completed"
                    && event["payload"]["status"] == expected_status
                    && json_contains_text(event, stdout_marker)
            }),
            "events lack bounded {expected_status} evidence: {events:#?}"
        );
        let report = fs::read_to_string(fixture.run_dir.join("report.md")).unwrap();
        assert!(
            report.contains(expected_status),
            "report lacks {expected_status}: {report}"
        );
    }
}

#[test]
fn hanging_paired_runner_aborts_with_bounded_partial_evidence() {
    let mut fixture = Fixture::new("paired-hang", &["c1"]);
    fixture.set_child_environment("FAKE_PAIRED_HANG", "1");
    fixture.set_limit("paired_timeout_ms", 1500);
    let started = Instant::now();
    let output = fixture.run(&[]);
    let elapsed = started.elapsed();
    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(
        elapsed < Duration::from_secs(5),
        "paired runner was not bounded: {elapsed:?}"
    );
    assert_evidence_bundle(&fixture);

    let stdout = fixture.raw_bytes("paired-aa-main.stdout");
    let stderr = fixture.raw_bytes("paired-aa-main.stderr");
    assert!(
        String::from_utf8_lossy(&stdout).contains("partial paired stdout"),
        "missing partial paired stdout: {}",
        String::from_utf8_lossy(&stdout)
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains("partial paired stderr"),
        "missing partial paired stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        stdout.len() <= 4096,
        "stdout was not capped: {}",
        stdout.len()
    );
    assert!(
        stderr.len() <= 4096,
        "stderr was not capped: {}",
        stderr.len()
    );

    let state = fixture.state();
    assert_eq!(state["status"], "failed", "state: {state}");
    assert_eq!(state["outcome"], "operational_failure", "state: {state}");
    assert!(json_contains_text(&state, "timeout"), "state: {state}");
    let events = fixture.events();
    assert!(
        events.iter().any(|event| {
            event["type"] == "command_completed"
                && event["payload"]["command_kind"] == "paired-aa"
                && event["payload"]["status"] == "timeout"
                && json_contains_text(event, "partial paired stdout")
        }),
        "events lack paired timeout evidence: {events:#?}"
    );
    let report = fs::read_to_string(fixture.run_dir.join("report.md")).unwrap();
    assert!(report.contains("timeout"), "report: {report}");
}

#[test]
fn overlapping_confirmation_design_is_rejected_before_measurement() {
    for (name, mutation) in [("overlapping-seeds", "seeds"), ("same-order-seed", "order")] {
        let mut fixture = Fixture::new(name, &["c1"]);
        match mutation {
            "seeds" => fixture.spec["confirmation"]["seeds"] = json!([42]),
            "order" => fixture.spec["confirmation"]["order_seed"] = json!(11),
            _ => unreachable!(),
        }
        fixture.rewrite_spec();
        let output = fixture.run(&[]);
        assert!(
            !output.status.success(),
            "case={name}: {}",
            output_text(&output)
        );
        assert!(
            fixture.bench_lines().is_empty(),
            "case={name}: bench ran before design validation"
        );
        assert!(
            fixture.paired_lines().is_empty(),
            "case={name}: paired ran before design validation"
        );
    }
}

#[test]
fn unknown_spec_field_is_rejected_before_measurement() {
    let mut fixture = Fixture::new("unknown-field", &["c1"]);
    fixture.spec["workloads"][0]["sesions"] = json!(2);
    fixture.rewrite_spec();

    let output = fixture.run(&[]);
    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(fixture.bench_lines().is_empty(), "bench ran unexpectedly");
    assert!(fixture.paired_lines().is_empty(), "paired ran unexpectedly");
}

#[test]
fn subunit_decision_threshold_is_rejected_before_measurement() {
    let mut fixture = Fixture::new("subunit-threshold", &["c1"]);
    fixture.spec["decision"]["min_lower_bound_ratio"] = json!(0.99);
    fixture.rewrite_spec();

    let output = fixture.run(&[]);
    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(fixture.bench_lines().is_empty(), "bench ran unexpectedly");
    assert!(fixture.paired_lines().is_empty(), "paired ran unexpectedly");
}

#[test]
fn existing_run_directory_is_rejected_without_touching_it() {
    let fixture = Fixture::new("existing-run", &["c1"]);
    fs::create_dir(&fixture.run_dir).unwrap();
    let marker = fixture.run_dir.join("keep-me");
    fs::write(&marker, "sentinel").unwrap();

    let output = fixture.run(&[]);
    assert!(!output.status.success(), "{}", output_text(&output));
    assert_eq!(fs::read_to_string(&marker).unwrap(), "sentinel");
    assert!(fixture.bench_lines().is_empty(), "bench ran unexpectedly");
    assert!(fixture.paired_lines().is_empty(), "paired ran unexpectedly");
    assert_eq!(fixture.root.join("run"), fixture.run_dir);
}
