// ABOUTME: Generic paired A/B and A/A subprocess runner.
// ABOUTME: It preregisters ABBA/BAAB blocks and analyzes paired log throughput ratios.
//
// Generalized from fenrin's `paired`: a "measure" is an opaque string the subject
// defines. Subject arguments can be forwarded either as a legacy opaque token blob
// or as exact argv entries. The runner never interprets either; it only checks that
// the record echoes the requested measure and the preregistered work.

use std::env;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use optikit::{
    approximate_power_blocks, parse_fixed_record, percentage, randomized_schedule, summarize,
    throughput, BlockOrder, FixedRecord, Label,
};

const USAGE: &str = "Usage: optikit-paired --baseline <bench-bin> --candidate <bench-bin> [options]\n       optikit-paired --aa <bench-bin> [options]\n\nOptions:\n  --measure <string>         Subject measurement path, forwarded opaque (default: scan)\n  --direct-args              Select exact argv transport, including with no argv entries\n  --subject-arg <arg>        Exact shared subject argv entry; repeat for more entries\n  --baseline-arg <arg>       Exact baseline argv entry; repeat to override shared entries\n  --candidate-arg <arg>      Exact candidate argv entry; repeat to override shared entries\n  --subject-args <tokens>    Legacy opaque shared argument blob (default: empty)\n  --baseline-args <tokens>   Legacy opaque baseline blob overriding the shared blob\n  --candidate-args <tokens>  Legacy opaque candidate blob overriding the shared blob\n  --count <integer>          Work units per fixed session (default: 10000)\n  --sessions <integer>       Timed sessions per process (default: 50)\n  --blocks <integer>         Randomized four-run blocks (default: 16; maximum: 100000)\n  --seed <integer>           Base work seed; repeat to cycle seeds (default: 42)\n  --order-seed <integer>     Reproducible schedule seed (default: 1)\n  --schedule <orders>        Explicit comma-separated ABBA/BAAB schedule\n  --timeout-ms <integer>     Per-process timeout in milliseconds (default: 300000)\n  --max-output-bytes <n>     Per-stream capture limit (default: 1048576; maximum: 67108864)\n  --target-speedup <percent> A/A power target (default: 3)\n  --held-out                 Label a fresh final confirmation run\n\nDirect argument options (`--direct-args` and `--*-arg`) cannot be mixed with any legacy `--*-args` option.";

const DEFAULT_MEASURE: &str = "scan";
const DEFAULT_COUNT: u64 = 10_000;
const DEFAULT_SESSIONS: u64 = 50;
const DEFAULT_BLOCKS: usize = 16;
const DEFAULT_WORK_SEED: u64 = 42;
const DEFAULT_ORDER_SEED: u64 = 1;
const DEFAULT_TARGET_SPEEDUP_PERCENT: f64 = 3.0;
const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;
const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BLOCKS: usize = 100_000;

#[derive(Debug, PartialEq)]
enum ForwardedArguments {
    Legacy {
        subject: String,
        baseline: Option<String>,
        candidate: Option<String>,
    },
    Direct {
        subject: Vec<String>,
        baseline: Option<Vec<String>>,
        candidate: Option<Vec<String>>,
    },
}

#[derive(Clone, Copy)]
enum EffectiveArguments<'a> {
    Legacy(&'a str),
    Direct(&'a [String]),
}

#[derive(Debug, PartialEq)]
struct Arguments {
    baseline: PathBuf,
    candidate: PathBuf,
    calibration: bool,
    measure: String,
    forwarded_args: ForwardedArguments,
    count: u64,
    sessions: u64,
    requested: u64,
    seeds: Vec<u64>,
    schedule: Vec<BlockOrder>,
    order_seed: Option<u64>,
    timeout_ms: u64,
    max_output_bytes: usize,
    target_speedup_percent: f64,
    held_out: bool,
}

impl Arguments {
    fn args_for(&self, label: Label) -> EffectiveArguments<'_> {
        match &self.forwarded_args {
            ForwardedArguments::Legacy {
                subject,
                baseline,
                candidate,
            } => EffectiveArguments::Legacy(match label {
                Label::Baseline => baseline.as_deref().unwrap_or(subject),
                Label::Candidate => candidate.as_deref().unwrap_or(subject),
            }),
            ForwardedArguments::Direct {
                subject,
                baseline,
                candidate,
            } => EffectiveArguments::Direct(match label {
                Label::Baseline => baseline.as_deref().unwrap_or(subject),
                Label::Candidate => candidate.as_deref().unwrap_or(subject),
            }),
        }
    }

    fn plan_args(&self) -> String {
        match (
            self.args_for(Label::Baseline),
            self.args_for(Label::Candidate),
        ) {
            (EffectiveArguments::Legacy(baseline), EffectiveArguments::Legacy(candidate)) => {
                format!(
                    "argument_transport=legacy baseline_subject_args={baseline:?} candidate_subject_args={candidate:?}"
                )
            }
            (EffectiveArguments::Direct(baseline), EffectiveArguments::Direct(candidate)) => {
                format!(
                    "argument_transport=direct baseline_argv={baseline:?} candidate_argv={candidate:?}"
                )
            }
            _ => unreachable!("both sides always use the same argument transport"),
        }
    }
}

fn parse_positive_u64(value: &str, description: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{description} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{description} must be a positive integer"));
    }
    Ok(parsed)
}

fn parse_schedule(value: &str) -> Result<Vec<BlockOrder>, String> {
    if value.is_empty() {
        return Err("schedule must contain ABBA or BAAB blocks".to_owned());
    }
    value.split(',').map(BlockOrder::parse).collect()
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Arguments>, String> {
    let mut baseline = None;
    let mut candidate = None;
    let mut aa = None;
    let mut measure = None;
    let mut subject_args = None;
    let mut baseline_args = None;
    let mut candidate_args = None;
    let mut subject_argv = Vec::new();
    let mut baseline_argv = Vec::new();
    let mut candidate_argv = Vec::new();
    let mut force_direct_args = false;
    let mut count = None;
    let mut sessions = None;
    let mut blocks = None;
    let mut seeds = Vec::new();
    let mut order_seed = None;
    let mut explicit_schedule: Option<String> = None;
    let mut timeout_ms = None;
    let mut max_output_bytes = None;
    let mut target_speedup_percent = None;
    let mut held_out = false;

    while let Some(argument) = args.next() {
        if matches!(argument.as_str(), "-h" | "--help") {
            return Ok(None);
        }

        let mut next = |option: &str| {
            args.next()
                .ok_or_else(|| format!("missing value after `{option}`"))
        };
        match argument.as_str() {
            "--baseline" => {
                if baseline.is_some() {
                    return Err("`--baseline` specified more than once".to_owned());
                }
                baseline = Some(PathBuf::from(next("--baseline")?));
            }
            "--candidate" => {
                if candidate.is_some() {
                    return Err("`--candidate` specified more than once".to_owned());
                }
                candidate = Some(PathBuf::from(next("--candidate")?));
            }
            "--aa" => {
                if aa.is_some() {
                    return Err("`--aa` specified more than once".to_owned());
                }
                aa = Some(PathBuf::from(next("--aa")?));
            }
            "--measure" => {
                if measure.is_some() {
                    return Err("`--measure` specified more than once".to_owned());
                }
                let value = next("--measure")?;
                if value.is_empty() {
                    return Err("`--measure` must not be empty".to_owned());
                }
                measure = Some(value);
            }
            "--direct-args" => {
                if force_direct_args {
                    return Err("`--direct-args` specified more than once".to_owned());
                }
                force_direct_args = true;
            }
            "--subject-args" => {
                if subject_args.is_some() {
                    return Err("`--subject-args` specified more than once".to_owned());
                }
                subject_args = Some(next("--subject-args")?);
            }
            "--baseline-args" => {
                if baseline_args.is_some() {
                    return Err("`--baseline-args` specified more than once".to_owned());
                }
                baseline_args = Some(next("--baseline-args")?);
            }
            "--candidate-args" => {
                if candidate_args.is_some() {
                    return Err("`--candidate-args` specified more than once".to_owned());
                }
                candidate_args = Some(next("--candidate-args")?);
            }
            "--subject-arg" => subject_argv.push(next("--subject-arg")?),
            "--baseline-arg" => baseline_argv.push(next("--baseline-arg")?),
            "--candidate-arg" => candidate_argv.push(next("--candidate-arg")?),
            "--count" => {
                if count.is_some() {
                    return Err("`--count` specified more than once".to_owned());
                }
                count = Some(parse_positive_u64(&next("--count")?, "count")?);
            }
            "--sessions" => {
                if sessions.is_some() {
                    return Err("`--sessions` specified more than once".to_owned());
                }
                sessions = Some(parse_positive_u64(&next("--sessions")?, "sessions")?);
            }
            "--blocks" => {
                if blocks.is_some() {
                    return Err("`--blocks` specified more than once".to_owned());
                }
                let parsed = parse_positive_u64(&next("--blocks")?, "blocks")?;
                blocks = Some(
                    usize::try_from(parsed)
                        .map_err(|_| "block count is too large for this platform".to_owned())?,
                );
            }
            "--seed" => {
                let value = next("--seed")?;
                seeds.push(
                    value
                        .parse::<u64>()
                        .map_err(|_| "seed must be a non-negative integer".to_owned())?,
                );
            }
            "--order-seed" => {
                if order_seed.is_some() {
                    return Err("`--order-seed` specified more than once".to_owned());
                }
                order_seed = Some(
                    next("--order-seed")?
                        .parse::<u64>()
                        .map_err(|_| "order seed must be a non-negative integer".to_owned())?,
                );
            }
            "--schedule" => {
                if explicit_schedule.is_some() {
                    return Err("`--schedule` specified more than once".to_owned());
                }
                explicit_schedule = Some(next("--schedule")?);
            }
            "--timeout-ms" => {
                if timeout_ms.is_some() {
                    return Err("`--timeout-ms` specified more than once".to_owned());
                }
                timeout_ms = Some(parse_positive_u64(&next("--timeout-ms")?, "timeout")?);
            }
            "--max-output-bytes" => {
                if max_output_bytes.is_some() {
                    return Err("`--max-output-bytes` specified more than once".to_owned());
                }
                let parsed =
                    parse_positive_u64(&next("--max-output-bytes")?, "maximum output bytes")?;
                if parsed > MAX_OUTPUT_BYTES {
                    return Err(format!(
                        "maximum output bytes must not exceed {MAX_OUTPUT_BYTES} (got {parsed})"
                    ));
                }
                max_output_bytes = Some(usize::try_from(parsed).map_err(|_| {
                    "maximum output bytes is too large for this platform".to_owned()
                })?);
            }
            "--target-speedup" => {
                if target_speedup_percent.is_some() {
                    return Err("`--target-speedup` specified more than once".to_owned());
                }
                let parsed = next("--target-speedup")?
                    .parse::<f64>()
                    .map_err(|_| "target speedup must be a positive percentage".to_owned())?;
                if !parsed.is_finite() || parsed <= 0.0 {
                    return Err("target speedup must be a positive percentage".to_owned());
                }
                target_speedup_percent = Some(parsed);
            }
            "--held-out" => {
                if held_out {
                    return Err("`--held-out` specified more than once".to_owned());
                }
                held_out = true;
            }
            _ => return Err(format!("unknown option `{argument}`")),
        }
    }

    let (baseline, candidate, calibration) = match (aa, baseline, candidate) {
        (Some(binary), None, None) => (binary.clone(), binary, true),
        (Some(_), _, _) => {
            return Err("`--aa` cannot be combined with `--baseline` or `--candidate`".to_owned());
        }
        (None, Some(baseline), Some(candidate)) => (baseline, candidate, false),
        (None, _, _) => {
            return Err("provide both `--baseline` and `--candidate`, or use `--aa`".to_owned());
        }
    };
    if calibration && held_out {
        return Err("`--held-out` cannot be combined with `--aa`".to_owned());
    }
    if calibration
        && (baseline_args.is_some()
            || candidate_args.is_some()
            || !baseline_argv.is_empty()
            || !candidate_argv.is_empty())
    {
        return Err(
            "per-side argument options cannot be combined with `--aa`; use `--subject-arg` or `--subject-args`"
                .to_owned(),
        );
    }

    let direct_args_specified = force_direct_args
        || !subject_argv.is_empty()
        || !baseline_argv.is_empty()
        || !candidate_argv.is_empty();
    let legacy_args_specified =
        subject_args.is_some() || baseline_args.is_some() || candidate_args.is_some();
    if direct_args_specified && legacy_args_specified {
        return Err(
            "direct argument options cannot be combined with legacy `--*-args` options".to_owned(),
        );
    }
    let forwarded_args = if direct_args_specified {
        ForwardedArguments::Direct {
            subject: subject_argv,
            baseline: (!baseline_argv.is_empty()).then_some(baseline_argv),
            candidate: (!candidate_argv.is_empty()).then_some(candidate_argv),
        }
    } else {
        ForwardedArguments::Legacy {
            subject: subject_args.unwrap_or_default(),
            baseline: baseline_args,
            candidate: candidate_args,
        }
    };

    // Validate fixed work before parsing or allocating the schedule and before
    // execute() has any opportunity to launch a subject process.
    let count = count.unwrap_or(DEFAULT_COUNT);
    let sessions = sessions.unwrap_or(DEFAULT_SESSIONS);
    let requested = count
        .checked_mul(sessions)
        .ok_or_else(|| "count times sessions overflowed".to_owned())?;

    let (schedule, registered_order_seed) = match explicit_schedule {
        Some(schedule_text) => {
            if blocks.is_some() || order_seed.is_some() {
                return Err(
                    "`--schedule` cannot be combined with `--blocks` or `--order-seed`".to_owned(),
                );
            }
            let block_count = schedule_text.split(',').count();
            if block_count > MAX_BLOCKS {
                return Err(format!(
                    "block count must not exceed {MAX_BLOCKS} (got {block_count})"
                ));
            }
            (parse_schedule(&schedule_text)?, None)
        }
        None => {
            let order_seed = order_seed.unwrap_or(DEFAULT_ORDER_SEED);
            let block_count = blocks.unwrap_or(DEFAULT_BLOCKS);
            if block_count > MAX_BLOCKS {
                return Err(format!(
                    "block count must not exceed {MAX_BLOCKS} (got {block_count})"
                ));
            }
            (
                randomized_schedule(block_count, order_seed),
                Some(order_seed),
            )
        }
    };
    if schedule.len() < 2 {
        return Err("at least two blocks are required for a confidence bound".to_owned());
    }
    if seeds.is_empty() {
        seeds.push(DEFAULT_WORK_SEED);
    }

    Ok(Some(Arguments {
        baseline,
        candidate,
        calibration,
        measure: measure.unwrap_or_else(|| DEFAULT_MEASURE.to_owned()),
        forwarded_args,
        count,
        sessions,
        requested,
        seeds,
        schedule,
        order_seed: registered_order_seed,
        timeout_ms: timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        max_output_bytes: max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
        target_speedup_percent: target_speedup_percent.unwrap_or(DEFAULT_TARGET_SPEEDUP_PERCENT),
        held_out,
    }))
}

fn compact_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.trim()
        .chars()
        .map(|character| {
            if matches!(character, '\n' | '\r' | '\t') {
                ' '
            } else {
                character
            }
        })
        .take(300)
        .collect()
}

fn classify_evidence(
    calibration: bool,
    held_out: bool,
    invalid_blocks: usize,
    lower_95_ratio: f64,
) -> &'static str {
    if invalid_blocks != 0 {
        "invalid_design"
    } else if calibration {
        "calibration_only"
    } else if held_out && lower_95_ratio > 1.0 {
        "candidate_faster"
    } else if held_out {
        "inconclusive"
    } else if lower_95_ratio > 1.0 {
        "screen_positive"
    } else {
        "screen_inconclusive"
    }
}

fn wire_f64(value: f64) -> String {
    // One leading digit plus sixteen fractional digits is sufficient to
    // round-trip every finite IEEE-754 binary64 value, while scientific
    // notation preserves very small and very large magnitudes.
    format!("{value:.16e}")
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

struct BoundedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

fn drain_bounded(mut reader: impl Read, max_output_bytes: usize) -> io::Result<BoundedOutput> {
    let mut bytes = Vec::with_capacity(max_output_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut exceeded = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let retained = read.min(max_output_bytes.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..retained]);
        exceeded |= retained < read;
    }
    Ok(BoundedOutput { bytes, exceeded })
}

#[cfg(unix)]
fn isolate_subject_process(command: &mut Command) {
    // A fresh process group lets timeout cleanup include descendants that inherit
    // the subject's stdout/stderr pipes.
    command.process_group(0);
}

#[cfg(not(unix))]
fn isolate_subject_process(_command: &mut Command) {}

#[cfg(unix)]
fn kill_subject_process_tree(child: &mut Child) -> io::Result<()> {
    let process_group = libc::pid_t::try_from(child.id()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "subject process ID does not fit pid_t",
        )
    })?;
    // SAFETY: `process_group` is the positive ID of the child group created
    // immediately before spawn. A negative PID asks kill(2) to signal that
    // group, and SIGKILL requires no signal handler or shared memory contract.
    let result = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        // The direct child and all descendants already exited.
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn kill_subject_process_tree(child: &mut Child) -> io::Result<()> {
    child.kill()
}

fn terminate_and_reap_subject(child: &mut Child, direct_child_reaped: bool) -> Result<(), String> {
    let tree_error = kill_subject_process_tree(child).err();
    if tree_error.is_some() && !direct_child_reaped {
        // If process-tree signaling failed, still make a best effort to prevent
        // leaking the direct child before waiting for it.
        let _ = child.kill();
    }
    let reap_error = if direct_child_reaped {
        None
    } else {
        child.wait().err()
    };
    if let Some(error) = reap_error {
        return Err(format!("could not reap direct child: {error}"));
    }
    if let Some(error) = tree_error {
        return Err(format!("could not terminate subject process tree: {error}"));
    }
    Ok(())
}

#[cfg(unix)]
fn finish_terminated_readers(
    stdout_reader: thread::JoinHandle<()>,
    stderr_reader: thread::JoinHandle<()>,
) -> Result<(), String> {
    // Processes remaining in the isolated group have been killed, so inherited
    // capture pipes close and both readers can drain to EOF.
    stdout_reader
        .join()
        .map_err(|_| "stdout reader panicked during cleanup".to_owned())?;
    stderr_reader
        .join()
        .map_err(|_| "stderr reader panicked during cleanup".to_owned())?;
    Ok(())
}

#[cfg(not(unix))]
fn finish_terminated_readers(
    stdout_reader: thread::JoinHandle<()>,
    stderr_reader: thread::JoinHandle<()>,
) -> Result<(), String> {
    // There is no portable std API for descendant-tree termination. Preserve
    // the timeout bound after killing and reaping the direct child.
    drop(stdout_reader);
    drop(stderr_reader);
    Ok(())
}

fn invoke(
    binary: &PathBuf,
    arguments: &Arguments,
    label: Label,
    seed: u64,
) -> Result<FixedRecord, String> {
    let measure = arguments.measure.as_str();
    let count = arguments.count;
    let sessions = arguments.sessions;
    let requested = arguments.requested;
    let timeout_ms = arguments.timeout_ms;
    let max_output_bytes = arguments.max_output_bytes;
    let timeout = Duration::from_millis(timeout_ms);
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "timeout is too large for this platform".to_owned())?;
    let mut command = Command::new(binary);
    command
        .arg("--measure")
        .arg(measure)
        .arg("--seed")
        .arg(seed.to_string())
        .arg("--sessions")
        .arg(sessions.to_string())
        .arg("--count")
        .arg(count.to_string());
    match arguments.args_for(label) {
        EffectiveArguments::Legacy(args) => {
            command.arg("--subject-args").arg(args);
        }
        EffectiveArguments::Direct(args) => {
            command.args(args);
        }
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    isolate_subject_process(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run {}: {error}", binary.display()))?;

    let child_stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let cleanup = terminate_and_reap_subject(&mut child, false);
            return Err(format!(
                "could not capture stdout from {}{}",
                binary.display(),
                cleanup
                    .err()
                    .map(|error| format!("; cleanup failed: {error}"))
                    .unwrap_or_default()
            ));
        }
    };
    let child_stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let cleanup = terminate_and_reap_subject(&mut child, false);
            return Err(format!(
                "could not capture stderr from {}{}",
                binary.display(),
                cleanup
                    .err()
                    .map(|error| format!("; cleanup failed: {error}"))
                    .unwrap_or_default()
            ));
        }
    };
    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    let stdout_reader = thread::Builder::new()
        .name("optikit-paired-stdout".to_owned())
        .spawn(move || {
            let result = drain_bounded(child_stdout, max_output_bytes);
            let _ = stdout_sender.send((OutputStream::Stdout, result));
        });
    let stdout_reader = match stdout_reader {
        Ok(reader) => reader,
        Err(error) => {
            let _ = terminate_and_reap_subject(&mut child, false);
            return Err(format!(
                "could not start stdout reader for {}: {error}",
                binary.display()
            ));
        }
    };
    let stderr_reader = thread::Builder::new()
        .name("optikit-paired-stderr".to_owned())
        .spawn(move || {
            let result = drain_bounded(child_stderr, max_output_bytes);
            let _ = sender.send((OutputStream::Stderr, result));
        });
    let stderr_reader = match stderr_reader {
        Ok(reader) => reader,
        Err(error) => {
            let _ = terminate_and_reap_subject(&mut child, false);
            #[cfg(unix)]
            let _ = stdout_reader.join();
            #[cfg(not(unix))]
            drop(stdout_reader);
            return Err(format!(
                "could not start stderr reader for {}: {error}",
                binary.display()
            ));
        }
    };

    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    loop {
        loop {
            match receiver.try_recv() {
                Ok((OutputStream::Stdout, result)) => stdout = Some(result),
                Ok((OutputStream::Stderr, result)) => stderr = Some(result),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(child_status) => status = child_status,
                Err(error) => {
                    let cleanup = terminate_and_reap_subject(&mut child, false);
                    let reader_cleanup = finish_terminated_readers(stdout_reader, stderr_reader);
                    return Err(format!(
                        "could not wait for {}: {error}{}{}",
                        binary.display(),
                        cleanup
                            .err()
                            .map(|error| format!("; process cleanup failed: {error}"))
                            .unwrap_or_default(),
                        reader_cleanup
                            .err()
                            .map(|error| format!("; reader cleanup failed: {error}"))
                            .unwrap_or_default(),
                    ));
                }
            }
        }
        if status.is_some() && stdout.is_some() && stderr.is_some() {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            // Terminate the group even if the direct child already exited: a
            // surviving in-group descendant may be keeping a capture pipe open.
            let cleanup = terminate_and_reap_subject(&mut child, status.is_some());
            let reader_cleanup = finish_terminated_readers(stdout_reader, stderr_reader);
            if let Err(error) = cleanup {
                return Err(format!(
                    "{} timed out after {timeout_ms} ms; cleanup failed: {error}",
                    binary.display()
                ));
            }
            if let Err(error) = reader_cleanup {
                return Err(format!(
                    "{} timed out after {timeout_ms} ms; reader cleanup failed: {error}",
                    binary.display()
                ));
            }
            return Err(format!(
                "{} timed out after {timeout_ms} ms",
                binary.display()
            ));
        }
        thread::sleep(Duration::from_millis(5).min(deadline.saturating_duration_since(now)));
    }

    // Receiving a result means each reader has finished, so joining cannot block.
    stdout_reader
        .join()
        .map_err(|_| format!("stdout reader for {} panicked", binary.display()))?;
    stderr_reader
        .join()
        .map_err(|_| format!("stderr reader for {} panicked", binary.display()))?;
    let stdout = stdout
        .expect("stdout capture checked above")
        .map_err(|error| format!("could not read stdout from {}: {error}", binary.display()))?;
    let stderr = stderr
        .expect("stderr capture checked above")
        .map_err(|error| format!("could not read stderr from {}: {error}", binary.display()))?;
    if stdout.exceeded {
        return Err(format!(
            "{} stdout exceeded --max-output-bytes limit of {max_output_bytes}",
            binary.display()
        ));
    }
    if stderr.exceeded {
        return Err(format!(
            "{} stderr exceeded --max-output-bytes limit of {max_output_bytes}",
            binary.display()
        ));
    }

    let status = status.expect("process status checked above");
    if !status.success() {
        return Err(format!(
            "{} exited with {}; stderr: {}",
            binary.display(),
            status,
            compact_output(&stderr.bytes)
        ));
    }
    let stdout = String::from_utf8(stdout.bytes)
        .map_err(|_| format!("{} emitted non-UTF-8 output", binary.display()))?;
    let record = parse_fixed_record(&stdout)?;
    if record.mode != measure {
        return Err(format!(
            "{} reported mode {} instead of {}",
            binary.display(),
            record.mode,
            measure
        ));
    }
    if record.seed != seed
        || record.count != count
        || record.sessions != sessions
        || record.warmup_sessions != 1
        || record.requested != requested
        || record.completed != requested
    {
        return Err(format!(
            "{} reported mismatched work (seed={}, count={}, sessions={}, warmup_sessions={}, requested={}, completed={})",
            binary.display(),
            record.seed,
            record.count,
            record.sessions,
            record.warmup_sessions,
            record.requested,
            record.completed
        ));
    }
    Ok(record)
}

fn execute(arguments: &Arguments) -> Result<bool, String> {
    let scope = if arguments.calibration {
        "calibration"
    } else if arguments.held_out {
        "held_out_confirmation"
    } else {
        "exploratory_per_candidate"
    };
    let schedule = arguments
        .schedule
        .iter()
        .map(|order| order.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let seeds = arguments
        .seeds
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let schedule_source = arguments
        .order_seed
        .map(|seed| format!("random:{seed}"))
        .unwrap_or_else(|| "explicit".to_owned());
    let plan_args = arguments.plan_args();
    println!(
        "PLAN experiment={} scope={} mode={} {} count={} sessions={} requested={} blocks={} order_source={} schedule={} seeds={} timeout_ms={} max_output_bytes_per_stream={}",
        if arguments.calibration { "AA" } else { "AB" },
        scope,
        arguments.measure,
        plan_args,
        arguments.count,
        arguments.sessions,
        arguments.requested,
        arguments.schedule.len(),
        schedule_source,
        schedule,
        seeds,
        arguments.timeout_ms,
        arguments.max_output_bytes,
    );
    io::stdout()
        .flush()
        .map_err(|error| format!("could not flush preregistered plan: {error}"))?;

    let mut log_ratios = Vec::with_capacity(arguments.schedule.len());
    let mut invalid_blocks = 0;

    for (block_index, &order) in arguments.schedule.iter().enumerate() {
        let seed = arguments.seeds[block_index % arguments.seeds.len()];
        let mut baseline_logs = Vec::with_capacity(2);
        let mut candidate_logs = Vec::with_capacity(2);
        let mut valid = true;

        for (position, label) in order.labels().into_iter().enumerate() {
            let binary = match label {
                Label::Baseline => &arguments.baseline,
                Label::Candidate => &arguments.candidate,
            };
            match invoke(binary, arguments, label, seed) {
                Ok(record) => {
                    let speed = throughput(&record);
                    println!(
                        "OBS block={} position={} label={} order={} seed={} elapsed_ns={} items_per_second={:.6} attempts={} output_bytes={}",
                        block_index + 1,
                        position + 1,
                        label.as_str(),
                        order,
                        seed,
                        record.elapsed_ns,
                        speed,
                        record.attempts,
                        record.output_bytes,
                    );
                    match label {
                        Label::Baseline => baseline_logs.push(speed.ln()),
                        Label::Candidate => candidate_logs.push(speed.ln()),
                    }
                }
                Err(error) => {
                    valid = false;
                    eprintln!(
                        "INVALID block={} position={} label={} order={} seed={}: {}",
                        block_index + 1,
                        position + 1,
                        label.as_str(),
                        order,
                        seed,
                        error,
                    );
                }
            }
        }

        if valid && baseline_logs.len() == 2 && candidate_logs.len() == 2 {
            let baseline_log = baseline_logs.iter().sum::<f64>() / 2.0;
            let candidate_log = candidate_logs.iter().sum::<f64>() / 2.0;
            let log_ratio = candidate_log - baseline_log;
            log_ratios.push(log_ratio);
            println!(
                "BLOCK block={} valid=true log_ratio={:.9} speedup_ratio={:.6} speedup_percent={:.3}",
                block_index + 1,
                log_ratio,
                log_ratio.exp(),
                percentage(log_ratio.exp()),
            );
        } else {
            invalid_blocks += 1;
            println!("BLOCK block={} valid=false", block_index + 1);
        }
    }

    let Some(summary) = summarize(&log_ratios) else {
        if invalid_blocks != 0 {
            println!(
                "RESULT experiment={} scope={} mode={} valid_blocks={} planned_blocks={} invalid_blocks={} evidence=invalid_design",
                if arguments.calibration { "AA" } else { "AB" },
                scope,
                arguments.measure,
                log_ratios.len(),
                arguments.schedule.len(),
                invalid_blocks,
            );
            return Ok(true);
        }
        return Err(format!(
            "only {} valid block(s); at least two are required",
            log_ratios.len()
        ));
    };
    println!(
        "RESULT experiment={} scope={} mode={} valid_blocks={} planned_blocks={} invalid_blocks={} mean_log_ratio={} log_ratio_sd={} speedup_ratio={} speedup_percent={} lower_95_one_sided_ratio={} lower_95_one_sided_percent={} evidence={}",
        if arguments.calibration { "AA" } else { "AB" },
        scope,
        arguments.measure,
        summary.blocks,
        arguments.schedule.len(),
        invalid_blocks,
        wire_f64(summary.mean_log_ratio),
        wire_f64(summary.log_ratio_sd),
        wire_f64(summary.estimate_ratio),
        wire_f64(percentage(summary.estimate_ratio)),
        wire_f64(summary.lower_95_ratio),
        wire_f64(percentage(summary.lower_95_ratio)),
        classify_evidence(
            arguments.calibration,
            arguments.held_out,
            invalid_blocks,
            summary.lower_95_ratio,
        ),
    );
    if arguments.calibration && invalid_blocks == 0 {
        println!(
            "CALIBRATION target_speedup_percent={:.3} approximate_blocks_for_80_percent_power={}",
            arguments.target_speedup_percent,
            approximate_power_blocks(summary.log_ratio_sd, arguments.target_speedup_percent),
        );
    }

    Ok(invalid_blocks != 0)
}

fn main() -> ExitCode {
    let arguments = match parse_args(env::args().skip(1)) {
        Ok(Some(arguments)) => arguments,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("optikit-paired: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match execute(&arguments) {
        Ok(false) => ExitCode::SUCCESS,
        Ok(true) => {
            eprintln!(
                "optikit-paired: one or more preregistered blocks were invalid; no runs were replaced"
            );
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("optikit-paired: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Result<Option<Arguments>, String> {
        parse_args(values.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn explicit_ab_and_aa_arguments_are_unambiguous() {
        let ab = arguments(&[
            "--baseline",
            "/tmp/a",
            "--candidate",
            "/tmp/b",
            "--measure",
            "scan",
            "--subject-args",
            "--impl thompson --corpus main.bin",
            "--count",
            "123",
            "--sessions",
            "7",
            "--seed",
            "7",
            "--seed",
            "9",
            "--schedule",
            "ABBA,BAAB",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(ab.baseline, PathBuf::from("/tmp/a"));
        assert_eq!(ab.candidate, PathBuf::from("/tmp/b"));
        assert!(!ab.calibration);
        assert_eq!(ab.measure, "scan");
        assert_eq!(
            ab.forwarded_args,
            ForwardedArguments::Legacy {
                subject: "--impl thompson --corpus main.bin".to_owned(),
                baseline: None,
                candidate: None,
            }
        );
        assert_eq!(ab.count, 123);
        assert_eq!(ab.sessions, 7);
        assert_eq!(ab.requested, 861);
        assert_eq!(ab.seeds, [7, 9]);
        assert_eq!(ab.schedule, [BlockOrder::Abba, BlockOrder::Baab]);
        assert_eq!(ab.order_seed, None);
        assert_eq!(ab.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(ab.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn parser_rejects_partial_or_conflicting_designs() {
        assert!(arguments(&[]).is_err());
        assert!(arguments(&["--baseline", "a"]).is_err());
        assert!(arguments(&["--aa", "a", "--baseline", "a", "--candidate", "b"]).is_err());
        assert!(arguments(&["--aa", "a", "--blocks", "1"]).is_err());
        assert!(
            arguments(&["--aa", "a", "--schedule", "ABBA,BAAB", "--order-seed", "2",]).is_err()
        );
        assert!(arguments(&["--aa", "a", "--count", "0"]).is_err());
        assert!(arguments(&["--aa", "a", "--sessions", "0"]).is_err());
        assert!(arguments(&["--aa", "a", "--held-out"]).is_err());
        assert!(arguments(&["--aa", "a", "--baseline-arg", "value"]).is_err());
        assert!(arguments(&["--aa", "a", "--timeout-ms", "0"]).is_err());
        assert!(arguments(&["--aa", "a", "--max-output-bytes", "0"]).is_err());
    }

    #[test]
    fn exact_argv_is_repeatable_and_cannot_mix_with_legacy_blobs() {
        let parsed = arguments(&[
            "--baseline",
            "/tmp/a",
            "--candidate",
            "/tmp/b",
            "--subject-arg",
            "--shared",
            "--subject-arg",
            "two words",
            "--candidate-arg",
            "--candidate-only",
            "--schedule",
            "ABBA,BAAB",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            parsed.forwarded_args,
            ForwardedArguments::Direct {
                subject: vec!["--shared".to_owned(), "two words".to_owned()],
                baseline: None,
                candidate: Some(vec!["--candidate-only".to_owned()]),
            }
        );

        let mixed = arguments(&[
            "--aa",
            "/tmp/a",
            "--subject-args",
            "legacy",
            "--subject-arg",
            "direct",
        ])
        .unwrap_err();
        assert!(mixed.contains("cannot be combined"), "error: {mixed}");
    }

    #[test]
    fn explicit_direct_transport_allows_empty_argv_and_rejects_conflicts() {
        let parsed = arguments(&["--aa", "/tmp/a", "--direct-args", "--schedule", "ABBA,BAAB"])
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.forwarded_args,
            ForwardedArguments::Direct {
                subject: Vec::new(),
                baseline: None,
                candidate: None,
            }
        );
        assert_eq!(
            parsed.plan_args(),
            "argument_transport=direct baseline_argv=[] candidate_argv=[]"
        );

        let duplicate =
            arguments(&["--aa", "/tmp/a", "--direct-args", "--direct-args"]).unwrap_err();
        assert!(
            duplicate.contains("specified more than once"),
            "error: {duplicate}"
        );

        for legacy_option in ["--subject-args", "--baseline-args", "--candidate-args"] {
            let mixed = arguments(&[
                "--baseline",
                "/tmp/a",
                "--candidate",
                "/tmp/b",
                "--direct-args",
                legacy_option,
                "legacy",
                "--schedule",
                "ABBA,BAAB",
            ])
            .unwrap_err();
            assert!(mixed.contains("cannot be combined"), "error: {mixed}");
        }
    }

    #[test]
    fn work_overflow_and_excessive_blocks_are_usage_errors() {
        let overflow = arguments(&[
            "--aa",
            "/tmp/a",
            "--count",
            &u64::MAX.to_string(),
            "--sessions",
            "2",
            "--schedule",
            "ABBA,BAAB",
        ])
        .unwrap_err();
        assert!(overflow.contains("overflowed"), "error: {overflow}");

        let too_many_blocks = (MAX_BLOCKS + 1).to_string();
        let excessive = arguments(&["--aa", "/tmp/a", "--blocks", &too_many_blocks]).unwrap_err();
        assert!(excessive.contains("must not exceed"), "error: {excessive}");

        let too_much_output = (MAX_OUTPUT_BYTES + 1).to_string();
        let excessive =
            arguments(&["--aa", "/tmp/a", "--max-output-bytes", &too_much_output]).unwrap_err();
        assert!(excessive.contains("must not exceed"), "error: {excessive}");
    }

    #[test]
    fn invalid_design_evidence_overrides_every_scientific_label() {
        assert_eq!(classify_evidence(false, false, 1, 2.0), "invalid_design");
        assert_eq!(classify_evidence(false, true, 1, 2.0), "invalid_design");
        assert_eq!(classify_evidence(true, false, 1, 2.0), "invalid_design");
    }

    #[test]
    fn wire_floats_round_trip_across_extreme_magnitudes() {
        for value in [
            -f64::MAX,
            -1.0e-300,
            -0.0,
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            1.0,
            1.000_000_59,
            1.0e300,
            f64::MAX,
        ] {
            let encoded = wire_f64(value);
            let decoded = encoded.parse::<f64>().unwrap();
            assert_eq!(decoded.to_bits(), value.to_bits(), "encoded: {encoded}");
        }
    }
}
