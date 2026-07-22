// ABOUTME: Scripted optimization-campaign driver for optiwork.
// ABOUTME: For one candidate it: preregisters the plan to LOG.md, runs the
// ABOUTME: equivalence gate, runs paired A/B on both corpora, applies the frozen
// ABOUTME: keep/reject rule, and appends the result + decision to LOG.md. The
// ABOUTME: candidate ladder itself lives in scripts/run-campaign.sh, which calls
// ABOUTME: this driver once per step.

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

const USAGE: &str = "\
optikit-campaign — drive one candidate through the gate + paired comparison.

  optikit-campaign \
--bench <bench-bin> --paired <optikit-paired-bin> \
--baseline-impl <name> --candidate-impl <name> \
--id <id> --hypothesis <text> --corpora-dir <dir> --log <LOG.md> \
--main-count <n> --main-sessions <n> --main-blocks <n> \
--pathological-count <n> --pathological-sessions <n> --pathological-blocks <n> \
[--order-seed <n>] [--seed <n>...] [--held-out]";

struct Config {
    bench: PathBuf,
    paired: PathBuf,
    baseline_impl: String,
    candidate_impl: String,
    id: String,
    hypothesis: String,
    corpora_dir: PathBuf,
    log: PathBuf,
    main_count: u64,
    main_sessions: u64,
    main_blocks: usize,
    path_count: u64,
    path_sessions: u64,
    path_blocks: usize,
    order_seed: u64,
    seeds: Vec<u64>,
    held_out: bool,
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        bench: PathBuf::new(),
        paired: PathBuf::new(),
        baseline_impl: String::new(),
        candidate_impl: String::new(),
        id: String::new(),
        hypothesis: String::new(),
        corpora_dir: PathBuf::new(),
        log: PathBuf::new(),
        main_count: 0,
        main_sessions: 0,
        main_blocks: 0,
        path_count: 0,
        path_sessions: 0,
        path_blocks: 0,
        order_seed: 1,
        seeds: Vec::new(),
        held_out: false,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut next = || {
            args.next()
                .ok_or_else(|| format!("missing value after `{arg}`"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Err("__help__".to_owned()),
            "--bench" => cfg.bench = PathBuf::from(next()?),
            "--paired" => cfg.paired = PathBuf::from(next()?),
            "--baseline-impl" => cfg.baseline_impl = next()?,
            "--candidate-impl" => cfg.candidate_impl = next()?,
            "--id" => cfg.id = next()?,
            "--hypothesis" => cfg.hypothesis = next()?,
            "--corpora-dir" => cfg.corpora_dir = PathBuf::from(next()?),
            "--log" => cfg.log = PathBuf::from(next()?),
            "--main-count" => {
                cfg.main_count = next()?.parse().map_err(|_| "bad --main-count".to_owned())?
            }
            "--main-sessions" => {
                cfg.main_sessions = next()?
                    .parse()
                    .map_err(|_| "bad --main-sessions".to_owned())?
            }
            "--main-blocks" => {
                cfg.main_blocks = next()?
                    .parse()
                    .map_err(|_| "bad --main-blocks".to_owned())?
            }
            "--pathological-count" => {
                cfg.path_count = next()?
                    .parse()
                    .map_err(|_| "bad --pathological-count".to_owned())?
            }
            "--pathological-sessions" => {
                cfg.path_sessions = next()?
                    .parse()
                    .map_err(|_| "bad --pathological-sessions".to_owned())?
            }
            "--pathological-blocks" => {
                cfg.path_blocks = next()?
                    .parse()
                    .map_err(|_| "bad --pathological-blocks".to_owned())?
            }
            "--order-seed" => {
                cfg.order_seed = next()?.parse().map_err(|_| "bad --order-seed".to_owned())?
            }
            "--seed" => cfg
                .seeds
                .push(next()?.parse().map_err(|_| "bad --seed".to_owned())?),
            "--held-out" => cfg.held_out = true,
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    if cfg.seeds.is_empty() {
        cfg.seeds.push(42);
    }
    for (field, name) in [
        (&cfg.bench, "bench"),
        (&cfg.paired, "paired"),
        (&cfg.corpora_dir, "corpora-dir"),
        (&cfg.log, "log"),
    ] {
        if field.as_os_str().is_empty() {
            return Err(format!("missing required --{name}"));
        }
    }
    for (field, name) in [
        (&cfg.baseline_impl, "baseline-impl"),
        (&cfg.candidate_impl, "candidate-impl"),
        (&cfg.id, "id"),
        (&cfg.hypothesis, "hypothesis"),
    ] {
        if field.is_empty() {
            return Err(format!("missing required --{name}"));
        }
    }
    Ok(cfg)
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn run(mut command: Command) -> Output {
    match command.output() {
        Ok(output) => Output {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => Output {
            ok: false,
            stdout: String::new(),
            stderr: format!("could not run command: {error}"),
        },
    }
}

/// Extract `key=value` from a whitespace-separated line.
fn field(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    line.split_whitespace()
        .find(|token| token.starts_with(&prefix))
        .and_then(|token| token.strip_prefix(&prefix))
        .map(str::to_owned)
}

/// Decision-relevant fields parsed from an optikit-paired RESULT line.
struct Verdict {
    lower_95_ratio: f64,
    speedup_percent: f64,
    evidence: String,
}

fn verdict_from(paired_stdout: &str) -> Result<Verdict, String> {
    let result_line = paired_stdout
        .lines()
        .find(|line| line.starts_with("RESULT"))
        .ok_or_else(|| "optikit-paired produced no RESULT line".to_owned())?;
    let lower = field(result_line, "lower_95_one_sided_ratio")
        .ok_or_else(|| "RESULT missing lower_95_one_sided_ratio".to_owned())?
        .parse::<f64>()
        .map_err(|_| "lower_95_one_sided_ratio is not a number".to_owned())?;
    let speedup_percent = field(result_line, "speedup_percent")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(f64::NAN);
    let evidence = field(result_line, "evidence").unwrap_or_default();
    Ok(Verdict {
        lower_95_ratio: lower,
        speedup_percent,
        evidence,
    })
}

fn run_gate(cfg: &Config, impl_name: &str, corpus: &str) -> Result<bool, String> {
    let mut cmd = Command::new(&cfg.bench);
    cmd.arg("--check")
        .arg(cfg.corpora_dir.join(format!("{corpus}.golden")))
        .arg("--impl")
        .arg(impl_name)
        .arg("--corpus")
        .arg(cfg.corpora_dir.join(format!("{corpus}.bin")));
    let out = run(cmd);
    if !out.stdout.is_empty() {
        println!("{}", out.stdout.trim_end());
    }
    if !out.ok {
        if !out.stderr.is_empty() {
            eprintln!("gate stderr: {}", out.stderr.trim());
        }
        return Err(format!(
            "gate failed for impl `{impl_name}` corpus `{corpus}`"
        ));
    }
    Ok(out.stdout.contains("PASS"))
}

fn run_paired(
    cfg: &Config,
    corpus: &str,
    count: u64,
    sessions: u64,
    blocks: usize,
) -> Result<Verdict, String> {
    let corpus_bin = cfg.corpora_dir.join(format!("{corpus}.bin"));
    let mut cmd = Command::new(&cfg.paired);
    cmd.arg("--baseline")
        .arg(&cfg.bench)
        .arg("--candidate")
        .arg(&cfg.bench)
        .arg("--measure")
        .arg("scan")
        .arg("--baseline-args")
        .arg(format!(
            "--impl {} --corpus {}",
            cfg.baseline_impl,
            corpus_bin.display()
        ))
        .arg("--candidate-args")
        .arg(format!(
            "--impl {} --corpus {}",
            cfg.candidate_impl,
            corpus_bin.display()
        ))
        .arg("--count")
        .arg(count.to_string())
        .arg("--sessions")
        .arg(sessions.to_string())
        .arg("--blocks")
        .arg(blocks.to_string())
        .arg("--order-seed")
        .arg(cfg.order_seed.to_string());
    for seed in &cfg.seeds {
        cmd.arg("--seed").arg(seed.to_string());
    }
    if cfg.held_out {
        cmd.arg("--held-out");
    }
    let out = run(cmd);
    if !out.stdout.is_empty() {
        println!("{}", out.stdout.trim_end());
    }
    if !out.ok {
        return Err(format!(
            "paired run failed for corpus `{corpus}`: {}",
            out.stderr.trim()
        ));
    }
    verdict_from(&out.stdout)
}

fn append(log: &PathBuf, text: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("could not open log: {e}"))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("could not write log: {e}"))?;
    Ok(())
}

fn gate_label(result: &Result<bool, String>) -> &'static str {
    match result {
        Ok(true) => "PASS",
        Ok(false) => "FAIL",
        Err(_) => "ERROR",
    }
}

fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(msg) if msg == "__help__" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("optikit-campaign: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let scope = if cfg.held_out {
        "held_out_confirmation"
    } else {
        "exploratory_per_candidate"
    };
    let commit = {
        let mut c = Command::new("git");
        c.args(["rev-parse", "--short", "HEAD"]);
        run(c).stdout.trim().to_owned()
    };
    let vcpus = run(Command::new("nproc")).stdout.trim().to_owned();

    let mut header = String::new();
    header.push_str(&format!(
        "\n## candidate: {} ({})\n",
        cfg.id, cfg.candidate_impl
    ));
    header.push_str(&format!("scope: {scope}\n"));
    header.push_str(&format!("baseline_impl: {}\n", cfg.baseline_impl));
    header.push_str(&format!("hypothesis: {}\n", cfg.hypothesis));
    header.push_str(&format!("commit: {commit}  vcpus: {vcpus}\n"));
    header.push_str(&format!(
        "PLAN: measure=scan main(count={} sessions={} blocks={}) pathological(count={} sessions={} blocks={}) order_seed={} seeds={:?}\n",
        cfg.main_count, cfg.main_sessions, cfg.main_blocks,
        cfg.path_count, cfg.path_sessions, cfg.path_blocks, cfg.order_seed, cfg.seeds
    ));
    if let Err(e) = append(&cfg.log, &header) {
        eprintln!("optikit-campaign: {e}");
        return ExitCode::FAILURE;
    }
    print!("{header}"); // echo the preregistered plan before any timing.

    // Gate: candidate must match the oracle on both corpora (fail-closed).
    let main_gate = run_gate(&cfg, &cfg.candidate_impl, "main");
    let path_gate = run_gate(&cfg, &cfg.candidate_impl, "pathological");
    let gates_ok = matches!(main_gate, Ok(true)) && matches!(path_gate, Ok(true));

    let mut body = String::new();
    let mut promoted = gates_ok;

    if !gates_ok {
        body.push_str(&format!(
            "gate: main={} pathological={}\n",
            gate_label(&main_gate),
            gate_label(&path_gate)
        ));
        body.push_str("Decision: rejected (equivalence gate)\n");
        // `promoted` is already `gates_ok` (false) here.
    } else {
        body.push_str("gate: main=PASS pathological=PASS\n");
        let main = run_paired(
            &cfg,
            "main",
            cfg.main_count,
            cfg.main_sessions,
            cfg.main_blocks,
        );
        let path = run_paired(
            &cfg,
            "pathological",
            cfg.path_count,
            cfg.path_sessions,
            cfg.path_blocks,
        );
        match (&main, &path) {
            (Ok(m), Ok(p)) => {
                body.push_str(&format!(
                    "main: lower_95_ratio={:.6} speedup_percent={:.3} evidence={}\n",
                    m.lower_95_ratio, m.speedup_percent, m.evidence
                ));
                body.push_str(&format!(
                    "pathological: lower_95_ratio={:.6} speedup_percent={:.3} evidence={}\n",
                    p.lower_95_ratio, p.speedup_percent, p.evidence
                ));
                // Frozen keep/reject rule: faster on BOTH corpora (lower 95% bound > 1).
                let main_ok = m.lower_95_ratio > 1.0;
                let path_ok = p.lower_95_ratio > 1.0;
                promoted = main_ok && path_ok;
                body.push_str(&format!(
                    "Decision: {} (main_95>1={main_ok} pathological_95>1={path_ok})\n",
                    if promoted { "promoted" } else { "rejected" }
                ));
            }
            _ => {
                body.push_str(&format!(
                    "Decision: rejected (paired run failed: main_err={:?} path_err={:?})\n",
                    main.as_ref().err(),
                    path.as_ref().err()
                ));
                promoted = false;
            }
        }
    }
    print!("{body}");
    if let Err(e) = append(&cfg.log, &body) {
        eprintln!("optikit-campaign: {e}");
        return ExitCode::FAILURE;
    }
    // Exit nonzero when the candidate was not promoted, so the script can branch.
    if promoted {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_extracts_token_value() {
        let line = "RESULT experiment=AB scope=x lower_95_one_sided_ratio=1.234 evidence=screen_positive foo=bar";
        assert_eq!(
            field(line, "lower_95_one_sided_ratio"),
            Some("1.234".to_owned())
        );
        assert_eq!(field(line, "evidence"), Some("screen_positive".to_owned()));
        assert_eq!(field(line, "missing"), None);
    }

    #[test]
    fn verdict_parses_result_line() {
        let stdout = "PLAN x\nBLOCK y\nRESULT experiment=AB lower_95_one_sided_ratio=0.5 speedup_percent=-50.0 evidence=screen_inconclusive\n";
        let v = verdict_from(stdout).unwrap();
        assert!((v.lower_95_ratio - 0.5).abs() < 1e-9);
        assert_eq!(v.evidence, "screen_inconclusive");
        assert!((v.speedup_percent - (-50.0)).abs() < 1e-9);
    }
}
