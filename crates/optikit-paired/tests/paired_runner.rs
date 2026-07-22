// ABOUTME: Integration tests for the optikit-paired runner against a scripted fake
// ABOUTME: subject: happy-path A/B and A/A, plus every fail-closed invalid-block path.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

/// A fake optiwork subject. It echoes the requested work back as a well-formed
/// `optiwork-fixed-v1` record with a deterministic `elapsed_ns`, so throughput
/// ratios are exact. Tokens in `--subject-args` select misbehaviors that exercise
/// the runner's fail-closed paths.
const FAKE_SUBJECT: &str = r#"#!/bin/sh
measure=""; seed=0; sessions=0; count=0; subject_args=""
while [ $# -gt 0 ]; do
  case "$1" in
    --measure) measure=$2; shift 2;;
    --seed) seed=$2; shift 2;;
    --sessions) sessions=$2; shift 2;;
    --count) count=$2; shift 2;;
    --subject-args) subject_args=$2; shift 2;;
    *) shift;;
  esac
done
elapsed=1000000
version=optiwork-fixed-v1
for tok in $subject_args; do
  case "$tok" in
    elapsed=*) elapsed=${tok#elapsed=};;
    badseed) seed=$((seed + 1));;
    badversion) version=old-version;;
    badmode) measure="not-$measure";;
    crash) exit 3;;
    crashonseed=*) if [ "$seed" = "${tok#crashonseed=}" ]; then exit 3; fi;;
  esac
done
requested=$((count * sessions))
printf '%s\tmode=%s\tseed=%s\tcount=%s\tsessions=%s\twarmup_sessions=1\trequested=%s\tcompleted=%s\tattempts=%s\telapsed_ns=%s\titems_per_second=0\toutput_bytes=0\n' \
  "$version" "$measure" "$seed" "$count" "$sessions" "$requested" "$requested" "$sessions" "$elapsed"
"#;

fn fake_subject(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("fake-subject-{name}.sh"));
    fs::write(&path, FAKE_SUBJECT).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn run_paired(args: &[&str]) -> (Output, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_optikit-paired"))
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    (output, stdout, stderr)
}

#[test]
fn ab_reports_exact_speedup_with_screen_positive_evidence() {
    let subject = fake_subject("ab");
    let subject = subject.to_str().unwrap();
    let (output, stdout, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--baseline-args",
        "elapsed=200000",
        "--candidate-args",
        "elapsed=100000",
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert!(output.status.success(), "stderr: {stderr}");
    // The preregistered plan must be printed before any observation.
    assert!(stdout.starts_with("PLAN experiment=AB"), "stdout: {stdout}");
    // Candidate elapsed is exactly half of baseline, so the ratio is exactly 2
    // in every block: zero variance, and the lower bound equals the estimate.
    assert!(stdout.contains("valid_blocks=2"), "stdout: {stdout}");
    assert!(
        stdout.contains("speedup_ratio=2.000000"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("lower_95_one_sided_ratio=2.000000"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("evidence=screen_positive"),
        "stdout: {stdout}"
    );
}

#[test]
fn held_out_run_is_labeled_and_confirms() {
    let subject = fake_subject("held-out");
    let subject = subject.to_str().unwrap();
    let (output, stdout, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--baseline-args",
        "elapsed=200000",
        "--candidate-args",
        "elapsed=100000",
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
        "--held-out",
    ]);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        stdout.contains("scope=held_out_confirmation"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("evidence=candidate_faster"),
        "stdout: {stdout}"
    );
}

#[test]
fn aa_calibration_reports_power_sizing() {
    let subject = fake_subject("aa");
    let (output, stdout, stderr) = run_paired(&[
        "--aa",
        subject.to_str().unwrap(),
        "--subject-args",
        "elapsed=100000",
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("experiment=AA"), "stdout: {stdout}");
    assert!(
        stdout.contains("evidence=calibration_only"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("CALIBRATION"), "stdout: {stdout}");
}

#[test]
fn mismatched_work_echo_fails_closed() {
    let subject = fake_subject("badseed");
    let subject = subject.to_str().unwrap();
    let (output, stdout, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--candidate-args",
        "badseed",
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr.contains("reported mismatched work"),
        "stderr: {stderr}"
    );
    assert!(
        stdout.contains("BLOCK block=1 valid=false"),
        "stdout: {stdout}"
    );
}

#[test]
fn wrong_record_version_fails_closed() {
    let subject = fake_subject("badversion");
    let subject = subject.to_str().unwrap();
    let (output, _, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--candidate-args",
        "badversion",
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr.contains("unsupported measurement record version"),
        "stderr: {stderr}"
    );
}

#[test]
fn wrong_mode_echo_fails_closed() {
    let subject = fake_subject("badmode");
    let subject = subject.to_str().unwrap();
    let (output, _, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--candidate-args",
        "badmode",
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert!(!output.status.success());
    assert!(stderr.contains("reported mode"), "stderr: {stderr}");
}

#[test]
fn crashing_subject_fails_closed() {
    let subject = fake_subject("crash");
    let subject = subject.to_str().unwrap();
    let (output, _, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--candidate-args",
        "crash",
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert!(!output.status.success());
    assert!(stderr.contains("exited with"), "stderr: {stderr}");
}

#[test]
fn invalid_blocks_are_dropped_never_replaced() {
    let subject = fake_subject("partial");
    let subject = subject.to_str().unwrap();
    // Seeds cycle per block: blocks 1 and 3 run seed 42 (valid), blocks 2 and 4
    // run seed 13, where the candidate crashes. The runner must analyze the two
    // valid blocks, report the two invalid ones, and still exit nonzero.
    let (output, stdout, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--baseline-args",
        "elapsed=200000",
        "--candidate-args",
        "elapsed=100000 crashonseed=13",
        "--count",
        "10",
        "--sessions",
        "2",
        "--seed",
        "42",
        "--seed",
        "13",
        "--schedule",
        "ABBA,BAAB,ABBA,BAAB",
    ]);
    assert!(!output.status.success());
    assert!(
        stdout.contains("BLOCK block=2 valid=false"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("valid_blocks=2"), "stdout: {stdout}");
    assert!(stdout.contains("invalid_blocks=2"), "stdout: {stdout}");
    assert!(stderr.contains("no runs were replaced"), "stderr: {stderr}");
}
