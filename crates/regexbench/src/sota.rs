// ABOUTME: Same-machine SOTA baseline — time the reference `regex` crate on
// ABOUTME: optiwork's exact corpus. Feature-gated (`oracle`) because the `regex`
// ABOUTME: crate IS the oracle: it never participates as an optimization candidate,
// ABOUTME: only as an external ceiling measured on the same machine/corpus.

//! Same-machine state-of-the-art baseline.
//!
//! The candidate matchers (`naive`, `thompson`, `prefilter`) are never linked
//! against the `regex` crate — the bench binary is built without the `oracle`
//! feature for every timed candidate. This module exists for the *other* question:
//! how fast is the real Rust `regex` engine (lazy/hybrid DFA + memchr/Teddy
//! prefilters) on optiwork's exact corpus and hardware? That number is the ceiling
//! the candidates chase.
//!
//! Because the `regex` crate compiled with `(?-u)` *is* the oracle, the spans
//! produced here are identical to the golden vectors by construction. So this is
//! not an optimization candidate (it would pass the equivalence gate vacuously) —
//! it is a reference measurement.

use crate::corpus::Pair;
use crate::Span;

/// A `regex`-crate pattern precompiled in `(?-u)` byte mode. Construction is always
/// outside the timed region; the lazy DFA itself is then warmed by the untimed
/// warmup scan, so the timed region measures matching, not compilation.
pub type CompiledPattern = regex::bytes::Regex;

/// Precompile every pair's pattern with the `regex` crate, `(?-u)` byte mode.
pub fn compile_all(pairs: &[Pair]) -> Result<Vec<CompiledPattern>, String> {
    pairs
        .iter()
        .map(|pair| {
            regex::bytes::Regex::new(&format!("(?-u){}", pair.pattern)).map_err(|error| {
                format!("regex crate rejected pattern `{}`: {error}", pair.pattern)
            })
        })
        .collect()
}

/// One pair's non-overlapping leftmost-first spans via the `regex` crate.
pub fn find_all(re: &CompiledPattern, input: &[u8], out: &mut Vec<Span>) {
    out.clear();
    for m in re.find_iter(input) {
        out.push(Span::new(m.start(), m.end()));
    }
}

/// Scan one pair by index, returning the bytes its matches cover. Mirrors the
/// candidate `scan_once` accounting so the record's `output_bytes` is comparable
/// across impl and `regex_crate`.
pub fn scan_pair(
    compiled: &[CompiledPattern],
    pairs: &[Pair],
    idx: usize,
    out: &mut Vec<Span>,
) -> u64 {
    find_all(&compiled[idx], &pairs[idx].input, out);
    let mut output_bytes = 0u64;
    for span in out.iter() {
        output_bytes += (span.end - span.start) as u64;
    }
    output_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(pattern: &str, input: &str) -> Vec<(usize, usize)> {
        let re = regex::bytes::Regex::new(&format!("(?-u){pattern}")).unwrap();
        let mut out = Vec::new();
        find_all(&re, input.as_bytes(), &mut out);
        out.iter().map(|s| (s.start, s.end)).collect()
    }

    #[test]
    fn matches_known_spans() {
        assert_eq!(spans("abc", "abcabc"), vec![(0, 3), (3, 6)]);
        assert_eq!(spans(r"\d+", "x42y7"), vec![(1, 3), (4, 5)]);
        // leftmost-first, same as the oracle
        assert_eq!(spans("a|ab", "ab"), vec![(0, 1)]);
    }
}
