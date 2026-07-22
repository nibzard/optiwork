// ABOUTME: Integration tests for the optikit-campaign driver against fake gate and
// ABOUTME: paired binaries: promotion, rejection, and the fail-closed gate path.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Fake bench binary. The campaign only uses it as the equivalence gate; it
/// passes every impl except `badcand`.
const FAKE_BENCH: &str = r#"#!/bin/sh
impl=""
while [ $# -gt 0 ]; do
  case "$1" in
    --impl) impl=$2; shift 2;;
    *) shift;;
  esac
done
if [ "$impl" = "badcand" ]; then
  echo "FAIL impl=$impl mismatched_pairs=1/1" >&2
  exit 1
fi
echo "PASS impl=$impl pairs=1 spans_checked"
"#;

/// Fake paired runner. Emits a RESULT line whose lower bound comes from
/// PAIRED_LOWER_MAIN / PAIRED_LOWER_PATH (per corpus), and touches
/// PAIRED_MARKER when set, so tests can assert whether timing ran at all.
const FAKE_PAIRED: &str = r#"#!/bin/sh
if [ -n "${PAIRED_MARKER:-}" ]; then : > "$PAIRED_MARKER"; fi
lower=${PAIRED_LOWER_MAIN:-1.5}
case "$*" in *pathological.bin*) lower=${PAIRED_LOWER_PATH:-1.5};; esac
printf 'PLAN fake\n'
printf 'RESULT experiment=AB scope=fake mode=scan lower_95_one_sided_ratio=%s speedup_percent=10.000 evidence=fake\n' "$lower"
"#;

fn write_script(name: &str, contents: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    fs::write(&path, contents).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn run_campaign(
    test: &str,
    candidate_impl: &str,
    env: &[(&str, &str)],
) -> (Output, String, PathBuf) {
    let bench = write_script(&format!("fake-bench-{test}.sh"), FAKE_BENCH);
    let paired = write_script(&format!("fake-paired-{test}.sh"), FAKE_PAIRED);
    let log = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("campaign-{test}.log.md"));
    let _ = fs::remove_file(&log);
    let mut command = Command::new(env!("CARGO_BIN_EXE_optikit-campaign"));
    command.args([
        "--bench",
        bench.to_str().unwrap(),
        "--paired",
        paired.to_str().unwrap(),
        "--baseline-impl",
        "naive",
        "--candidate-impl",
        candidate_impl,
        "--id",
        test,
        "--hypothesis",
        "fake hypothesis",
        "--corpora-dir",
        env!("CARGO_TARGET_TMPDIR"),
        "--log",
        log.to_str().unwrap(),
        "--main-count",
        "10",
        "--main-sessions",
        "2",
        "--main-blocks",
        "2",
        "--pathological-count",
        "5",
        "--pathological-sessions",
        "1",
        "--pathological-blocks",
        "2",
    ]);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    let log_text = fs::read_to_string(&log).unwrap();
    (output, log_text, log)
}

fn marker_path(test: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("paired-ran-{test}"));
    let _ = fs::remove_file(&path);
    path
}

fn assert_preregistration(log_text: &str, id: &str, impl_name: &str) {
    assert!(
        log_text.contains(&format!("## candidate: {id} ({impl_name})")),
        "log: {log_text}"
    );
    assert!(
        log_text.contains("hypothesis: fake hypothesis"),
        "log: {log_text}"
    );
    assert!(log_text.contains("PLAN: measure=scan"), "log: {log_text}");
}

#[test]
fn promoted_when_lower_bound_beats_one_on_both_corpora() {
    let (output, log_text, _) = run_campaign("promote", "fastcand", &[]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_preregistration(&log_text, "promote", "fastcand");
    assert!(
        log_text.contains("gate: main=PASS pathological=PASS"),
        "log: {log_text}"
    );
    assert!(
        log_text.contains("Decision: promoted (main_95>1=true pathological_95>1=true)"),
        "log: {log_text}"
    );
}

#[test]
fn rejected_when_one_corpus_regresses() {
    let (output, log_text, _) = run_campaign("reject", "fastcand", &[("PAIRED_LOWER_MAIN", "0.9")]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        log_text.contains("Decision: rejected (main_95>1=false pathological_95>1=true)"),
        "log: {log_text}"
    );
}

#[test]
fn gate_failure_rejects_before_any_timing() {
    let marker = marker_path("gate");
    let (output, log_text, _) = run_campaign(
        "gate",
        "badcand",
        &[("PAIRED_MARKER", marker.to_str().unwrap())],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        log_text.contains("Decision: rejected (equivalence gate)"),
        "log: {log_text}"
    );
    // Fail-closed: the paired runner must never have been invoked.
    assert!(
        !Path::new(&marker).exists(),
        "paired ran despite a failing gate"
    );
}
