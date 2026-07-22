// ABOUTME: Integration tests for the optikit-paired runner against a scripted fake
// ABOUTME: subject: happy-path A/B and A/A, plus every fail-closed invalid-block path.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

/// A fake optiwork subject. It echoes the requested work back as a well-formed
/// `optiwork-fixed-v1` record with a deterministic `elapsed_ns`, so throughput
/// ratios are exact. Tokens in `--subject-args` select misbehaviors that exercise
/// the runner's fail-closed paths. The `protocol-only` measure rejects the legacy
/// transport entirely so tests can verify that no `--subject-args` option was sent.
const FAKE_SUBJECT: &str = r#"#!/bin/sh
measure=""; seed=0; sessions=0; count=0; subject_args=""; elapsed=1000000
stdout_bytes=0; stderr_bytes=0
while [ $# -gt 0 ]; do
  case "$1" in
    --measure) measure=$2; shift 2;;
    --seed) seed=$2; shift 2;;
    --sessions) sessions=$2; shift 2;;
    --count) count=$2; shift 2;;
    --subject-args)
      if [ "$measure" = "protocol-only" ]; then
        printf '%s\n' 'protocol-only subject received --subject-args' >&2
        exit 64
      fi
      subject_args=$2
      shift 2
      ;;
    --elapsed) elapsed=$2; shift 2;;
    --sleep) sleep "$2"; shift 2;;
    --spawn-child) sleep "$2" & descendant_pid=$!; printf '%s\n' "$descendant_pid" >> "$3"; wait "$descendant_pid"; shift 3;;
    --stdout-bytes) stdout_bytes=$2; shift 2;;
    --stderr-bytes) stderr_bytes=$2; shift 2;;
    *) shift;;
  esac
done
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
i=0
while [ "$i" -lt "$stdout_bytes" ]; do printf x; i=$((i + 1)); done
i=0
while [ "$i" -lt "$stderr_bytes" ]; do printf x >&2; i=$((i + 1)); done
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

fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal zero performs an existence/permission check and does not
    // modify the process identified by this exact PID.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn assert_process_exits(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_exists(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_exists(pid),
        "descendant PID {pid} survived timeout"
    );
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
        stdout.contains("speedup_ratio=2.0000000000000000e0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("lower_95_one_sided_ratio=2.0000000000000000e0"),
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
fn direct_arguments_are_forwarded_as_exact_argv_entries() {
    let subject = fake_subject("direct-argv");
    let subject = subject.to_str().unwrap();
    let (output, stdout, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--baseline-arg",
        "--elapsed",
        "--baseline-arg",
        "200000",
        "--candidate-arg",
        "--elapsed",
        "--candidate-arg",
        "100000",
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        stdout.contains("argument_transport=direct"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("baseline_argv=[\"--elapsed\", \"200000\"]"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("candidate_argv=[\"--elapsed\", \"100000\"]"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("speedup_ratio=2.000000"),
        "stdout: {stdout}"
    );
}

#[test]
fn empty_direct_transport_omits_legacy_subject_args() {
    let subject = fake_subject("empty-direct");
    let subject = subject.to_str().unwrap();
    let (output, stdout, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--measure",
        "protocol-only",
        "--direct-args",
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        stdout.contains(
            "mode=protocol-only argument_transport=direct baseline_argv=[] candidate_argv=[]"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn subject_timeout_kills_the_run_and_fails_closed() {
    let subject = fake_subject("timeout");
    let pid_file = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("timeout-descendants.pids");
    let _ = fs::remove_file(&pid_file);
    let subject = subject.to_str().unwrap();
    let pid_file_arg = pid_file.to_str().unwrap();
    let (output, stdout, stderr) = run_paired(&[
        "--baseline",
        subject,
        "--candidate",
        subject,
        "--candidate-arg",
        "--spawn-child",
        "--candidate-arg",
        "30",
        "--candidate-arg",
        pid_file_arg,
        "--count",
        "10",
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
        "--timeout-ms",
        "250",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr.contains("timed out after 250 ms"),
        "stderr: {stderr}"
    );
    assert!(stdout.contains("valid_blocks=0"), "stdout: {stdout}");
    assert!(
        stdout.contains("evidence=invalid_design"),
        "stdout: {stdout}"
    );
    let descendant_pids = fs::read_to_string(&pid_file)
        .unwrap()
        .lines()
        .map(|line| line.parse::<libc::pid_t>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(descendant_pids.len(), 4, "pids: {descendant_pids:?}");
    for pid in descendant_pids {
        assert_process_exits(pid);
    }
}

#[test]
fn stdout_and_stderr_capture_limits_fail_closed() {
    let subject = fake_subject("output-limit");
    let subject = subject.to_str().unwrap();
    for (stream, direct_option) in [("stdout", "--stdout-bytes"), ("stderr", "--stderr-bytes")] {
        let (output, stdout, stderr) = run_paired(&[
            "--baseline",
            subject,
            "--candidate",
            subject,
            "--candidate-arg",
            direct_option,
            "--candidate-arg",
            "1000",
            "--count",
            "10",
            "--sessions",
            "2",
            "--schedule",
            "ABBA,BAAB",
            "--max-output-bytes",
            "256",
        ]);
        assert!(!output.status.success());
        assert!(
            stdout.contains("evidence=invalid_design"),
            "stdout: {stdout}"
        );
        assert!(
            stderr.contains(&format!(
                "{stream} exceeded --max-output-bytes limit of 256"
            )),
            "stderr: {stderr}"
        );
    }
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
    assert!(
        stdout.contains("evidence=invalid_design"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("evidence=screen_positive"),
        "stdout: {stdout}"
    );
    assert!(stderr.contains("no runs were replaced"), "stderr: {stderr}");
}

#[test]
fn overflowing_fixed_work_is_rejected_before_process_launch() {
    let missing_subject = "/optikit-test-subject-that-must-not-exist";
    let max_count = u64::MAX.to_string();
    let (output, stdout, stderr) = run_paired(&[
        "--baseline",
        missing_subject,
        "--candidate",
        missing_subject,
        "--count",
        &max_count,
        "--sessions",
        "2",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(
        stderr.contains("count times sessions overflowed"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("could not run"), "stderr: {stderr}");
}

#[test]
fn excessive_output_limit_is_rejected_before_process_launch() {
    let missing_subject = "/optikit-test-subject-that-must-not-exist";
    let (output, stdout, stderr) = run_paired(&[
        "--baseline",
        missing_subject,
        "--candidate",
        missing_subject,
        "--max-output-bytes",
        "67108865",
        "--schedule",
        "ABBA,BAAB",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(
        stderr.contains("maximum output bytes must not exceed 67108864"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("could not run"), "stderr: {stderr}");
}
