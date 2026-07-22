// ABOUTME: Reference oracle for regexbench, built on the `regex` crate.
// ABOUTME: Feature-gated (`oracle`) so it links ONLY into the offline golden-vector
// ABOUTME: generator, never into the timed bench binary. The pattern is compiled with
// ABOUTME: `(?-u)` to match this crate's ASCII/byte semantics exactly.

//! Oracle spans via the reference `regex` crate.
//!
//! The bench binary and the candidate matchers never depend on this; it exists only
//! to generate golden vectors offline. A second trusted engine cross-check lives in
//! the generator (and a differential test in `tests/`), so a `regex`-crate bug in
//! leftmost-first matching cannot silently bless a wrong candidate.

use crate::corpus::{GoldenPair, Pair};
use crate::Span;

/// Compute the oracle's non-overlapping leftmost-first spans for one pair.
///
/// `(?-u)` selects ASCII classes and byte-based `.`, matching this crate's
/// `ByteClass` definitions, and disables Unicode scalar handling.
pub fn oracle_spans(pattern: &str, input: &[u8]) -> Result<Vec<Span>, String> {
    let compiled = regex::bytes::Regex::new(&format!("(?-u){pattern}"))
        .map_err(|error| format!("oracle rejected pattern `{pattern}`: {error}"))?;
    Ok(compiled
        .find_iter(input)
        .map(|m| Span::new(m.start(), m.end()))
        .collect())
}

/// Lift a corpus of pairs to golden vectors by computing oracle spans per pair.
pub fn golden_for(pairs: &[Pair]) -> Result<Vec<GoldenPair>, String> {
    pairs
        .iter()
        .map(|pair| {
            let spans = oracle_spans(&pair.pattern, &pair.input)?;
            Ok(GoldenPair {
                pattern: pair.pattern.clone(),
                input: pair.input.clone(),
                spans,
            })
        })
        .collect()
}
