// ABOUTME: regexbench bench binary — the optiwork subject.
// ABOUTME: Emits one `optiwork-fixed-v1` record per run for `optikit-paired`, scans
// ABOUTME: a corpus through a chosen impl with fixed work (count*sessions bytes), and
// ABOUTME: doubles as an equivalence gate (`--check <golden>`) and a corpus-size
// ABOUTME: helper (`--count-of <path>`). Depends only on std + regexbench, never on
// ABOUTME: the harness or the oracle crate.

use std::env;
use std::fs;
use std::process::ExitCode;
use std::time::Instant;

use regexbench::corpus::{corpus_bytes, deserialize_corpus, deserialize_golden, Pair};
use regexbench::{Impl, Regex, Span};

/// Wire-format version echoed in every record. Duplicated (not imported from
/// optikit) so this binary stays free of harness dependencies; a mismatch is caught
/// by the runner's version check.
const FIXED_RECORD_VERSION: &str = "optiwork-fixed-v1";
const GATE_RECORD_VERSION: &str = "optiwork-gate-v1";

const USAGE: &str = "\
regexbench bench binary (optiwork subject)

Timed fixed-work run (driven by optikit-paired):
  bench --measure <mode> --seed <n> --sessions <n> --count <n> --subject-args \"--impl <name> --corpus <path>\"

Equivalence gate:
  bench --check <golden> --impl <name> --corpus <path> \
    --optiwork-gate-artifact-id <id> --optiwork-gate-workload-id <id>

Corpus byte count (for the campaign to size --count):
  bench --count-of <path>";

struct Config {
    impl_name: Option<String>,
    corpus: Option<String>,
    check: Option<String>,
    count_of: Option<String>,
    measure: Option<String>,
    seed: Option<u64>,
    sessions: Option<u64>,
    count: Option<u64>,
    subject_args: Option<String>,
    gate_artifact_id: Option<String>,
    gate_workload_id: Option<String>,
}

impl Config {
    fn new() -> Self {
        Self {
            impl_name: None,
            corpus: None,
            check: None,
            count_of: None,
            measure: None,
            seed: None,
            sessions: None,
            count: None,
            subject_args: None,
            gate_artifact_id: None,
            gate_workload_id: None,
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<Config, String> {
    let mut config = Config::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let mut take = || -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("missing value after `{arg}`"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Err("__help__".to_owned()),
            "--impl" => config.impl_name = Some(take()?),
            "--corpus" => config.corpus = Some(take()?),
            "--check" => config.check = Some(take()?),
            "--count-of" => config.count_of = Some(take()?),
            "--measure" => config.measure = Some(take()?),
            "--seed" => {
                config.seed = Some(
                    take()?
                        .parse()
                        .map_err(|_| "seed must be an integer".to_owned())?,
                )
            }
            "--sessions" => {
                config.sessions = Some(
                    take()?
                        .parse()
                        .map_err(|_| "sessions must be an integer".to_owned())?,
                )
            }
            "--count" => {
                config.count = Some(
                    take()?
                        .parse()
                        .map_err(|_| "count must be an integer".to_owned())?,
                )
            }
            "--subject-args" => config.subject_args = Some(take()?),
            "--optiwork-gate-artifact-id" => config.gate_artifact_id = Some(take()?),
            "--optiwork-gate-workload-id" => config.gate_workload_id = Some(take()?),
            other => return Err(format!("unknown option `{other}`")),
        }
        i += 1;
    }

    // Merge opaque subject-args: optikit-paired forwards --impl/--corpus here.
    if let Some(blob) = config.subject_args.as_deref() {
        let mut sub = blob.split_whitespace();
        while let Some(flag) = sub.next() {
            match flag {
                "--impl" => {
                    config.impl_name = sub.next().map(str::to_owned);
                }
                "--corpus" => {
                    config.corpus = sub.next().map(str::to_owned);
                }
                _ => {}
            }
        }
    }
    Ok(config)
}

/// SplitMix64-derived RNG, seeded per run so the pair-order permutation is
/// reproducible from the runner's `--seed`.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Fisher-Yates shuffle of `[0, n)`.
    fn permute(&mut self, n: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..n).collect();
        for j in (1..n).rev() {
            let k = (self.next_u64() as usize) % (j + 1);
            order.swap(j, k);
        }
        order
    }
}

fn load_corpus(path: &str) -> Result<Vec<Pair>, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read corpus `{path}`: {error}"))?;
    deserialize_corpus(&bytes)
}

fn compile_all(pairs: &[Pair]) -> Result<Vec<Regex>, String> {
    pairs.iter().map(|pair| Regex::new(&pair.pattern)).collect()
}

/// One full scan of every pair in a (seeded) permuted order.
fn scan_once(
    impl_: Impl,
    compiled: &[Regex],
    pairs: &[Pair],
    rng: &mut Rng,
    out: &mut Vec<Span>,
) -> (u64, u64) {
    let order = rng.permute(pairs.len());
    let mut attempts = 0u64;
    let mut output_bytes = 0u64;
    for &idx in &order {
        out.clear();
        impl_.find_all(&compiled[idx], &pairs[idx].input, out);
        attempts += 1;
        for span in out.iter() {
            output_bytes += (span.end - span.start) as u64;
        }
    }
    (attempts, output_bytes)
}

fn run_timed(config: &Config) -> Result<(), String> {
    let impl_name = config
        .impl_name
        .as_deref()
        .ok_or_else(|| "timed run requires `--impl`".to_owned())?;
    let impl_ = Impl::parse(impl_name)?;
    let corpus_path = config
        .corpus
        .as_deref()
        .ok_or_else(|| "timed run requires `--corpus`".to_owned())?;
    let pairs = load_corpus(corpus_path)?;
    if pairs.is_empty() {
        return Err("corpus contains no pairs".to_owned());
    }
    let compiled = compile_all(&pairs)?;

    let sessions = config
        .sessions
        .ok_or_else(|| "missing `--sessions`".to_owned())?;
    let seed = config.seed.ok_or_else(|| "missing `--seed`".to_owned())?;
    let count = config.count.ok_or_else(|| "missing `--count`".to_owned())?;
    let mode = config.measure.clone().unwrap_or_else(|| "scan".to_owned());

    let total_bytes = corpus_bytes(&pairs);
    if total_bytes != count {
        return Err(format!(
            "corpus has {total_bytes} bytes but `--count` was {count}; the runner's work check would fail"
        ));
    }
    let requested = count
        .checked_mul(sessions)
        .ok_or_else(|| "count times sessions overflowed".to_owned())?;

    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    // Warmup: one untimed scan.
    scan_once(impl_, &compiled, &pairs, &mut rng, &mut out);

    let start = Instant::now();
    let mut attempts = 0u64;
    let mut output_bytes = 0u64;
    for _ in 0..sessions {
        let (a, b) = scan_once(impl_, &compiled, &pairs, &mut rng, &mut out);
        attempts += a;
        output_bytes += b;
    }
    let elapsed_ns = start.elapsed().as_nanos();
    if elapsed_ns == 0 {
        return Err("timed region took zero nanoseconds".to_owned());
    }
    let items_per_second = requested as f64 * 1_000_000_000.0 / elapsed_ns as f64;

    println!(
        "{FIXED_RECORD_VERSION}\tmode={mode}\tseed={seed}\tcount={count}\tsessions={sessions}\twarmup_sessions=1\trequested={requested}\tcompleted={requested}\tattempts={attempts}\telapsed_ns={elapsed_ns}\titems_per_second={items_per_second}\toutput_bytes={output_bytes}",
    );
    Ok(())
}

fn run_check(config: &Config) -> Result<bool, String> {
    let impl_name = config
        .impl_name
        .as_deref()
        .ok_or_else(|| "gate run requires `--impl`".to_owned())?;
    let impl_ = Impl::parse(impl_name)?;
    let corpus_path = config
        .corpus
        .as_deref()
        .ok_or_else(|| "gate run requires `--corpus`".to_owned())?;
    let golden_path = config
        .check
        .as_deref()
        .ok_or_else(|| "gate run requires `--check`".to_owned())?;
    let gate_artifact_id = config
        .gate_artifact_id
        .as_deref()
        .ok_or_else(|| "gate run requires `--optiwork-gate-artifact-id`".to_owned())?;
    let gate_workload_id = config
        .gate_workload_id
        .as_deref()
        .ok_or_else(|| "gate run requires `--optiwork-gate-workload-id`".to_owned())?;

    let corpus = load_corpus(corpus_path)?;
    if corpus.is_empty() {
        return Err("gate corpus contains no pairs".to_owned());
    }
    let golden_bytes =
        fs::read(golden_path).map_err(|e| format!("could not read golden `{golden_path}`: {e}"))?;
    let golden = deserialize_golden(&golden_bytes)?;
    if golden.len() != corpus.len() {
        return Err(format!(
            "corpus has {} pairs but golden has {}; they do not match",
            corpus.len(),
            golden.len()
        ));
    }

    let mut mismatches = 0usize;
    let mut out = Vec::new();
    for (idx, pair) in corpus.iter().enumerate() {
        let re = Regex::new(&pair.pattern)?;
        out.clear();
        impl_.find_all(&re, &pair.input, &mut out);
        if out != golden[idx].spans {
            mismatches += 1;
            if mismatches <= 5 {
                eprintln!(
                    "MISMATCH impl={impl_name} pair={idx} pattern={:?} input_len={} expected={} got={}",
                    pair.pattern,
                    pair.input.len(),
                    golden[idx].spans.len(),
                    out.len()
                );
            }
        }
    }
    if mismatches == 0 {
        println!(
            "{GATE_RECORD_VERSION}\tstatus=equivalent\tartifact_id={gate_artifact_id}\tworkload_id={gate_workload_id}\tchecked_units={}",
            corpus.len()
        );
        Ok(true)
    } else {
        eprintln!(
            "FAIL impl={impl_name} mismatched_pairs={mismatches}/{}",
            corpus.len()
        );
        Ok(false)
    }
}

fn run_count_of(config: &Config) -> Result<(), String> {
    let path = config
        .count_of
        .as_deref()
        .ok_or_else(|| "`--count-of` requires a path".to_owned())?;
    let pairs = load_corpus(path)?;
    println!("{}", corpus_bytes(&pairs));
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let config = match parse_args(args) {
        Ok(config) => config,
        Err(msg) if msg == "__help__" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("regexbench-bench: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let result = if config.count_of.is_some() {
        run_count_of(&config).map(|_| true)
    } else if config.check.is_some() {
        run_check(&config)
    } else {
        run_timed(&config).map(|_| true)
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("regexbench-bench: {error}");
            // Exit 1 is reserved for a valid equivalence mismatch. Configuration,
            // input, and execution errors are operational failures for the campaign.
            ExitCode::from(2)
        }
    }
}
