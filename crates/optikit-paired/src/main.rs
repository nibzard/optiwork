// ABOUTME: Generic dependency-free paired A/B and A/A runner.
// ABOUTME: It preregisters ABBA/BAAB blocks and analyzes paired log throughput ratios.
//
// Generalized from fenrin's `paired`: a "measure" is an opaque string the subject
// defines, and `--subject-args` is an opaque token blob forwarded verbatim to the
// subject bench binary. The runner never interprets either; it only checks that the
// record echoes the requested measure and the preregistered work.

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use optikit::{
    approximate_power_blocks, parse_fixed_record, percentage, randomized_schedule, summarize,
    throughput, BlockOrder, FixedRecord, Label,
};

const USAGE: &str = "Usage: optikit-paired --baseline <bench-bin> --candidate <bench-bin> [options]\n       optikit-paired --aa <bench-bin> [options]\n\nOptions:\n  --measure <string>         Subject measurement path, forwarded opaque (default: scan)\n  --subject-args <tokens>    Opaque args forwarded to the subject binary (default: empty)\n  --count <integer>          Work units per fixed session (default: 10000)\n  --sessions <integer>       Timed sessions per process (default: 50)\n  --blocks <integer>         Randomized four-run blocks (default: 16)\n  --seed <integer>           Base work seed; repeat to cycle seeds (default: 42)\n  --order-seed <integer>     Reproducible schedule seed (default: 1)\n  --schedule <orders>        Explicit comma-separated ABBA/BAAB schedule\n  --target-speedup <percent> A/A power target (default: 3)\n  --held-out                 Label a fresh final confirmation run";

const DEFAULT_MEASURE: &str = "scan";
const DEFAULT_COUNT: u64 = 10_000;
const DEFAULT_SESSIONS: u64 = 50;
const DEFAULT_BLOCKS: usize = 16;
const DEFAULT_WORK_SEED: u64 = 42;
const DEFAULT_ORDER_SEED: u64 = 1;
const DEFAULT_TARGET_SPEEDUP_PERCENT: f64 = 3.0;

#[derive(Debug, PartialEq)]
struct Arguments {
    baseline: PathBuf,
    candidate: PathBuf,
    calibration: bool,
    measure: String,
    subject_args: String,
    /// Optional per-side overrides. When set, the baseline side receives
    /// `baseline_args` and the candidate side `candidate_args`; otherwise both
    /// fall back to `subject_args`. This lets one bench binary compare two of its
    /// own configurations (e.g. `--impl naive` vs `--impl thompson`).
    baseline_args: Option<String>,
    candidate_args: Option<String>,
    count: u64,
    sessions: u64,
    seeds: Vec<u64>,
    schedule: Vec<BlockOrder>,
    order_seed: Option<u64>,
    target_speedup_percent: f64,
    held_out: bool,
}

impl Arguments {
    /// The opaque args forwarded to the bench binary for a given side.
    fn args_for(&self, label: Label) -> &str {
        match label {
            Label::Baseline => self.baseline_args.as_deref().unwrap_or(&self.subject_args),
            Label::Candidate => self.candidate_args.as_deref().unwrap_or(&self.subject_args),
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
    let mut count = None;
    let mut sessions = None;
    let mut blocks = None;
    let mut seeds = Vec::new();
    let mut order_seed = None;
    let mut explicit_schedule = None;
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
                explicit_schedule = Some(parse_schedule(&next("--schedule")?)?);
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
    if calibration && (baseline_args.is_some() || candidate_args.is_some()) {
        return Err(
            "`--baseline-args`/`--candidate-args` cannot be combined with `--aa`".to_owned(),
        );
    }

    let (schedule, registered_order_seed) = match explicit_schedule {
        Some(schedule) => {
            if blocks.is_some() || order_seed.is_some() {
                return Err(
                    "`--schedule` cannot be combined with `--blocks` or `--order-seed`".to_owned(),
                );
            }
            (schedule, None)
        }
        None => {
            let order_seed = order_seed.unwrap_or(DEFAULT_ORDER_SEED);
            (
                randomized_schedule(blocks.unwrap_or(DEFAULT_BLOCKS), order_seed),
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
        subject_args: subject_args.unwrap_or_default(),
        baseline_args,
        candidate_args,
        count: count.unwrap_or(DEFAULT_COUNT),
        sessions: sessions.unwrap_or(DEFAULT_SESSIONS),
        seeds,
        schedule,
        order_seed: registered_order_seed,
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

fn invoke(
    binary: &PathBuf,
    measure: &str,
    subject_args: &str,
    count: u64,
    sessions: u64,
    seed: u64,
) -> Result<FixedRecord, String> {
    let mut command = Command::new(binary);
    command
        .arg("--measure")
        .arg(measure)
        .arg("--seed")
        .arg(seed.to_string())
        .arg("--sessions")
        .arg(sessions.to_string())
        .arg("--count")
        .arg(count.to_string())
        .arg("--subject-args")
        .arg(subject_args);
    let output = command
        .output()
        .map_err(|error| format!("could not run {}: {error}", binary.display()))?;

    if !output.status.success() {
        return Err(format!(
            "{} exited with {}; stderr: {}",
            binary.display(),
            output.status,
            compact_output(&output.stderr)
        ));
    }
    let stdout = String::from_utf8(output.stdout)
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
    let requested = count
        .checked_mul(sessions)
        .ok_or_else(|| "count times sessions overflowed".to_owned())?;
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
    let per_side = match (&arguments.baseline_args, &arguments.candidate_args) {
        (Some(b), Some(c)) => format!(" baseline_args=\"{b}\" candidate_args=\"{c}\""),
        _ => String::new(),
    };
    println!(
        "PLAN experiment={} scope={} mode={} subject_args=\"{}\"{per_side} count={} sessions={} blocks={} order_source={} schedule={} seeds={}",
        if arguments.calibration { "AA" } else { "AB" },
        scope,
        arguments.measure,
        arguments.subject_args,
        arguments.count,
        arguments.sessions,
        arguments.schedule.len(),
        schedule_source,
        schedule,
        seeds,
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
            match invoke(
                binary,
                &arguments.measure,
                arguments.args_for(label),
                arguments.count,
                arguments.sessions,
                seed,
            ) {
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

    let summary = summarize(&log_ratios).ok_or_else(|| {
        format!(
            "only {} valid block(s); at least two are required",
            log_ratios.len()
        )
    })?;
    println!(
        "RESULT experiment={} scope={} mode={} valid_blocks={} planned_blocks={} invalid_blocks={} mean_log_ratio={:.9} log_ratio_sd={:.9} speedup_ratio={:.6} speedup_percent={:.3} lower_95_one_sided_ratio={:.6} lower_95_one_sided_percent={:.3} evidence={}",
        if arguments.calibration { "AA" } else { "AB" },
        scope,
        arguments.measure,
        summary.blocks,
        arguments.schedule.len(),
        invalid_blocks,
        summary.mean_log_ratio,
        summary.log_ratio_sd,
        summary.estimate_ratio,
        percentage(summary.estimate_ratio),
        summary.lower_95_ratio,
        percentage(summary.lower_95_ratio),
        if arguments.calibration {
            "calibration_only"
        } else if arguments.held_out && summary.lower_95_ratio > 1.0 {
            "candidate_faster"
        } else if arguments.held_out {
            "inconclusive"
        } else if summary.lower_95_ratio > 1.0 {
            "screen_positive"
        } else {
            "screen_inconclusive"
        },
    );
    if arguments.calibration {
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
        assert_eq!(ab.subject_args, "--impl thompson --corpus main.bin");
        assert_eq!(ab.count, 123);
        assert_eq!(ab.sessions, 7);
        assert_eq!(ab.seeds, [7, 9]);
        assert_eq!(ab.schedule, [BlockOrder::Abba, BlockOrder::Baab]);
        assert_eq!(ab.order_seed, None);
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
    }
}
