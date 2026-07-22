// ABOUTME: Generic optimization-measurement primitives extracted from fenrin.
// ABOUTME: Versioned fixed-work records, ABBA/BAAB scheduling, and paired-statistics.
//
// This crate is pure: no I/O, no subprocesses, no subject-specific knowledge.
// A "mode" is an opaque string a subject bench binary defines (e.g. "scan",
// "findfirst"). The runner only checks that the record echoes the mode it asked for.

#![forbid(unsafe_code)]

/// Wire-format version for the fixed-work measurement record.
///
/// Bumped from fenrin's `fenrin-fixed-v1` because the recorded throughput field
/// was renamed (`names_per_second` -> `items_per_second`) and the mode vocabulary
/// is no longer fixed. A pre-bump binary cannot participate in an optiwork campaign,
/// matching fenrin's own protocol rule.
pub const FIXED_RECORD_VERSION: &str = "optiwork-fixed-v1";

/// 95% one-sided standard-normal quantile (z for P(Z <= z) = 0.95).
pub const NORMAL_95_ONE_SIDED: f64 = 1.644_853_626_951_472_2;
/// 80% power standard-normal quantile (z for P(Z <= z) = 0.80).
pub const NORMAL_80_POWER: f64 = 0.841_621_233_572_914_3;

/// Which side of a paired comparison an observation belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    Baseline,
    Candidate,
}

impl Label {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "A",
            Self::Candidate => "B",
        }
    }
}

/// A four-observation block order. Counterbalancing cancels monotonic drift
/// (thermal, background load) within a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockOrder {
    Abba,
    Baab,
}

impl BlockOrder {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_uppercase().as_str() {
            "ABBA" => Ok(Self::Abba),
            "BAAB" => Ok(Self::Baab),
            _ => Err(format!(
                "invalid block order `{value}`; expected `ABBA` or `BAAB`"
            )),
        }
    }

    pub fn labels(self) -> [Label; 4] {
        match self {
            Self::Abba => [
                Label::Baseline,
                Label::Candidate,
                Label::Candidate,
                Label::Baseline,
            ],
            Self::Baab => [
                Label::Candidate,
                Label::Baseline,
                Label::Baseline,
                Label::Candidate,
            ],
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Abba => "ABBA",
            Self::Baab => "BAAB",
        }
    }
}

impl std::fmt::Display for BlockOrder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A parsed fixed-work measurement record emitted by a subject bench binary.
///
/// The subject also emits a computed throughput field (`items_per_second`) on the
/// same line; the runner ignores it and recomputes throughput from `completed` and
/// `elapsed_ns` so the number cannot be faked by the subject.
#[derive(Debug, PartialEq, Eq)]
pub struct FixedRecord {
    pub mode: String,
    pub seed: u64,
    pub count: u64,
    pub sessions: u64,
    pub warmup_sessions: u64,
    pub requested: u64,
    pub completed: u64,
    pub attempts: u64,
    pub elapsed_ns: u128,
    pub output_bytes: u64,
}

pub fn record_field<'a>(fields: &'a [&'a str], key: &str) -> Result<&'a str, String> {
    let prefix = format!("{key}=");
    let mut matches = fields
        .iter()
        .filter_map(|field| field.strip_prefix(&prefix));
    let value = matches
        .next()
        .ok_or_else(|| format!("measurement record is missing `{key}`"))?;
    if matches.next().is_some() {
        return Err(format!("measurement record repeats `{key}`"));
    }
    Ok(value)
}

/// Parse exactly one `optiwork-fixed-v1` record line. Rejects the wrong version,
/// extra lines, missing/repeated fields, non-integer numerics, and zero elapsed time.
pub fn parse_fixed_record(text: &str) -> Result<FixedRecord, String> {
    let mut lines = text.lines();
    let line = lines
        .next()
        .ok_or_else(|| "benchmark produced no measurement record".to_owned())?;
    if lines.next().is_some() {
        return Err("benchmark produced more than one output line".to_owned());
    }
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.first().copied() != Some(FIXED_RECORD_VERSION) {
        return Err(format!(
            "unsupported measurement record version `{}`",
            fields.first().copied().unwrap_or("")
        ));
    }

    let parse_u64 = |key: &str| -> Result<u64, String> {
        record_field(&fields, key)?
            .parse::<u64>()
            .map_err(|_| format!("measurement field `{key}` is not an integer"))
    };
    let parse_u128 = |key: &str| -> Result<u128, String> {
        record_field(&fields, key)?
            .parse::<u128>()
            .map_err(|_| format!("measurement field `{key}` is not an integer"))
    };

    let record = FixedRecord {
        mode: record_field(&fields, "mode")?.to_owned(),
        seed: parse_u64("seed")?,
        count: parse_u64("count")?,
        sessions: parse_u64("sessions")?,
        warmup_sessions: parse_u64("warmup_sessions")?,
        requested: parse_u64("requested")?,
        completed: parse_u64("completed")?,
        attempts: parse_u64("attempts")?,
        elapsed_ns: parse_u128("elapsed_ns")?,
        output_bytes: parse_u64("output_bytes")?,
    };
    if record.elapsed_ns == 0 {
        return Err("measurement elapsed time must be greater than zero".to_owned());
    }
    Ok(record)
}

/// Deterministic schedule RNG (SplitMix64-derived). Reproducible from a seed so the
/// whole ABBA/BAAB plan can be printed before any process starts.
pub struct ScheduleRng(u64);

impl ScheduleRng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

/// Generate `blocks` randomized ABBA/BAAB orders deterministically from `seed`.
pub fn randomized_schedule(blocks: usize, seed: u64) -> Vec<BlockOrder> {
    let mut rng = ScheduleRng::new(seed);
    (0..blocks)
        .map(|_| {
            if rng.next_u64() & 1 == 0 {
                BlockOrder::Abba
            } else {
                BlockOrder::Baab
            }
        })
        .collect()
}

/// Throughput recomputed from the record (items per second). The subject's own
/// emitted throughput field is never trusted.
pub fn throughput(record: &FixedRecord) -> f64 {
    record.completed as f64 * 1_000_000_000.0 / record.elapsed_ns as f64
}

/// Paired comparison summary across blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub blocks: usize,
    pub mean_log_ratio: f64,
    pub log_ratio_sd: f64,
    pub estimate_ratio: f64,
    pub lower_95_ratio: f64,
}

/// One-sided 95% t critical value. Exact table for small degrees of freedom
/// (1..=30); Cornish-Fisher expansion beyond.
pub fn t_critical_95_one_sided(degrees_of_freedom: usize) -> f64 {
    const SMALL_DF: [f64; 30] = [
        6.314, 2.920, 2.353, 2.132, 2.015, 1.943, 1.895, 1.860, 1.833, 1.812, 1.796, 1.782, 1.771,
        1.761, 1.753, 1.746, 1.740, 1.734, 1.729, 1.725, 1.721, 1.717, 1.714, 1.711, 1.708, 1.706,
        1.703, 1.701, 1.699, 1.697,
    ];
    if degrees_of_freedom <= SMALL_DF.len() {
        return SMALL_DF[degrees_of_freedom - 1];
    }

    let df = degrees_of_freedom as f64;
    let z = NORMAL_95_ONE_SIDED;
    z + (z.powi(3) + z) / (4.0 * df)
        + (5.0 * z.powi(5) + 16.0 * z.powi(3) + 3.0 * z) / (96.0 * df.powi(2))
        + (3.0 * z.powi(7) + 19.0 * z.powi(5) + 17.0 * z.powi(3) - 15.0 * z) / (384.0 * df.powi(3))
}

/// Summarize per-block paired log-ratios into a geometric speedup estimate and a
/// one-sided 95% lower bound. Returns `None` for fewer than two ratios.
pub fn summarize(log_ratios: &[f64]) -> Option<Summary> {
    if log_ratios.len() < 2 {
        return None;
    }
    let blocks = log_ratios.len();
    let mean = log_ratios.iter().sum::<f64>() / blocks as f64;
    let squared_deviations = log_ratios
        .iter()
        .map(|ratio| (ratio - mean).powi(2))
        .sum::<f64>();
    let standard_deviation = (squared_deviations / (blocks - 1) as f64).sqrt();
    let standard_error = standard_deviation / (blocks as f64).sqrt();
    let lower_log = mean - t_critical_95_one_sided(blocks - 1) * standard_error;

    Some(Summary {
        blocks,
        mean_log_ratio: mean,
        log_ratio_sd: standard_deviation,
        estimate_ratio: mean.exp(),
        lower_95_ratio: lower_log.exp(),
    })
}

/// Approximate block count for 80% power at a target speedup given an observed
/// block-log-ratio standard deviation. Used by A/A calibration to size an A/B run
/// *before* any candidate is seen.
pub fn approximate_power_blocks(log_ratio_sd: f64, target_speedup_percent: f64) -> usize {
    let target_log_ratio = (1.0 + target_speedup_percent / 100.0).ln();
    let estimate = ((NORMAL_95_ONE_SIDED + NORMAL_80_POWER) * log_ratio_sd / target_log_ratio)
        .powi(2)
        .ceil();
    if estimate.is_finite() {
        (estimate as usize).max(2)
    } else {
        usize::MAX
    }
}

pub fn percentage(ratio: f64) -> f64 {
    (ratio - 1.0) * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn randomized_schedule_is_reproducible() {
        let first = randomized_schedule(32, 91);
        let second = randomized_schedule(32, 91);
        let other = randomized_schedule(32, 92);

        assert_eq!(first, second);
        assert_ne!(first, other);
        assert!(first.contains(&BlockOrder::Abba));
        assert!(first.contains(&BlockOrder::Baab));
    }

    #[test]
    fn record_parser_checks_version_and_fields() {
        let text = "optiwork-fixed-v1\tmode=scan\tseed=42\tcount=200000\tsessions=50\twarmup_sessions=1\trequested=10000000\tcompleted=10000000\tattempts=1234\telapsed_ns=500000000\titems_per_second=20000000.0\toutput_bytes=8000\n";
        assert_eq!(
            parse_fixed_record(text),
            Ok(FixedRecord {
                mode: "scan".to_owned(),
                seed: 42,
                count: 200_000,
                sessions: 50,
                warmup_sessions: 1,
                requested: 10_000_000,
                completed: 10_000_000,
                attempts: 1234,
                elapsed_ns: 500_000_000,
                output_bytes: 8000,
            })
        );
        assert!(parse_fixed_record(&text.replacen("optiwork-fixed-v1", "old", 1)).is_err());
        assert!(parse_fixed_record(&text.replace("elapsed_ns=500000000", "elapsed_ns=0")).is_err());
        assert!(parse_fixed_record(&format!("{text}extra\n")).is_err());
    }

    #[test]
    fn record_parser_rejects_wrong_mode_echo_is_subject_business() {
        // The parser stores whatever mode string the subject emits; the runner,
        // not the parser, checks that it matches what was requested.
        let text = "optiwork-fixed-v1\tmode=whatever\tseed=1\tcount=1\tsessions=1\twarmup_sessions=1\trequested=1\tcompleted=1\tattempts=0\telapsed_ns=1\toutput_bytes=0\n";
        assert_eq!(parse_fixed_record(text).unwrap().mode, "whatever");
    }

    #[test]
    fn summary_uses_geometric_speedup_and_a_one_sided_bound() {
        let constant = vec![1.1_f64.ln(); 4];
        let summary = summarize(&constant).unwrap();

        assert!((summary.mean_log_ratio - 1.1_f64.ln()).abs() < 1e-12);
        assert!((summary.estimate_ratio - 1.1).abs() < 1e-12);
        assert!((summary.lower_95_ratio - 1.1).abs() < 1e-12);
        assert_eq!(summary.log_ratio_sd, 0.0);
    }

    #[test]
    fn aa_power_estimate_grows_with_noise_and_smaller_effects() {
        let low_noise = approximate_power_blocks(0.01, 3.0);
        let high_noise = approximate_power_blocks(0.05, 3.0);
        let smaller_effect = approximate_power_blocks(0.05, 1.0);

        assert!(high_noise > low_noise);
        assert!(smaller_effect > high_noise);
    }
}
