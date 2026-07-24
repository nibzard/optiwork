// ABOUTME: Literal-prefix prefilter matcher (leftmost-first, greedy).
// ABOUTME: Scans for a required leading literal with SIMD `memchr::memmem`, then
// ABOUTME: verifies each hit with the linear Thompson engine (anchored). Patterns
// ABOUTME: with no usable literal prefix fall through to the full Thompson scan, so
// ABOUTME: this is never slower than catastrophic backtracking and faster wherever a
// SELECTIVE literal exists.

//! Literal-prefix prefilter.
//!
//! Many patterns start with a run of literal bytes (`needle`, `abc`, …). Any
//! match of such a pattern must begin at an occurrence of that literal, so we can
//! use a vectorized substring search (`memchr::memmem`) to jump straight to the
//! handful of candidate start positions and skip the bytes in between. Each
//! candidate is then verified with an *anchored* run of the Pike VM, which is
//! linear-time — so the prefilter is correct and never hits exponential
//! backtracking, even on patterns like `needle(blah)+x`.
//!
//! Patterns without a leading literal prefix of length ≥ 2 (quantifiers,
//! alternation, classes, single characters, anchors at the front) get no benefit
//! from this and are delegated to the plain Thompson scan. The corpus is built so
//! that the one large haystack pair (`needle` in 8 KiB of filler) exercises the
//! prefilter and dominates the byte count.

use crate::thompson;
use crate::{Node, Regex, Span};

/// Scan the input for all non-overlapping matches using a literal-prefix prefilter.
pub fn find_all(re: &Regex, input: &[u8], out: &mut Vec<Span>) {
    match leading_literal_prefix(&re.ast) {
        Some(prefix) if prefix.len() >= 2 => {
            let prog = thompson::Program::new(&re.ast);
            crate::scan_matches(
                input.len(),
                |scan| find_with_prefix(&prog, input, &prefix, scan),
                out,
            );
        }
        // No selective literal: behave exactly like the Thompson engine.
        _ => thompson::find_all(re, input, out),
    }
}

/// Find the leftmost match at or after `scan` by jumping between literal hits.
fn find_with_prefix(
    prog: &thompson::Program,
    input: &[u8],
    prefix: &[u8],
    scan: usize,
) -> Option<(usize, usize)> {
    let mut pos = scan;
    loop {
        // Next occurrence of the literal at or after `pos`. None means no further
        // match is possible, since every match must start with the literal.
        let rel = memchr::memmem::find(&input[pos..], prefix)?;
        let start = pos + rel;
        // Anchored verify: the match, if any, begins exactly at `start`.
        if let Some((s, end)) = thompson::pike_find(prog, input, start, false) {
            return Some((s, end));
        }
        pos = start + 1;
    }
}

/// The maximal run of single literal bytes at the very start of the pattern, or
/// `None` if the pattern does not begin with a literal. A single-character prefix
/// is returned as a one-byte vector; callers require length ≥ 2 before activating.
pub(crate) fn leading_literal_prefix(ast: &Node) -> Option<Vec<u8>> {
    let parts: &[Node] = match ast {
        Node::Concat(parts) => parts,
        Node::Class(class) => return class.single_byte().map(|b| vec![b]),
        _ => return None,
    };
    let mut prefix = Vec::new();
    for node in parts {
        match node {
            Node::Class(class) => match class.single_byte() {
                Some(byte) => prefix.push(byte),
                None => break,
            },
            _ => break,
        }
    }
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Impl, Regex};

    fn spans(impl_: Impl, re: &str, input: &str) -> Vec<(usize, usize)> {
        let compiled = Regex::new(re).unwrap();
        let mut out = Vec::new();
        impl_.find_all(&compiled, input.as_bytes(), &mut out);
        out.iter().map(|s| (s.start, s.end)).collect()
    }

    #[test]
    fn prefilter_matches_naive_on_prefix_patterns() {
        // Patterns with a leading literal — these activate the prefilter.
        let cases: &[(&str, &str)] = &[
            ("needle", "xneedle neex needle y"),
            ("abc", "abcabc zzz abc"),
            ("ab", "ababab"),
            ("ca", "café café"), // multibyte: 'c','a' are the leading literal bytes
            ("xyzq", "no match here"),
        ];
        for (pattern, input) in cases {
            let naive = spans(Impl::Naive, pattern, input);
            let pref = spans(Impl::Prefilter, pattern, input);
            assert_eq!(naive, pref, "pattern={pattern:?} input={input:?}");
        }
    }

    #[test]
    fn prefilter_matches_naive_on_delegated_patterns() {
        // Patterns without a ≥2 literal prefix delegate to Thompson.
        let cases: &[(&str, &str)] = &[
            ("a+", "aaaa"),
            ("a*", "xx"),
            ("(ab)+", "abababx"),
            (".*", "abc"),
            ("a|ab", "abab"),
            ("^a", "ab"),
        ];
        for (pattern, input) in cases {
            let naive = spans(Impl::Naive, pattern, input);
            let pref = spans(Impl::Prefilter, pattern, input);
            assert_eq!(naive, pref, "pattern={pattern:?} input={input:?}");
        }
    }

    #[test]
    fn prefilter_handles_overlapping_literal_runs() {
        // "aa" in "aaaa" matches at 0 and 2 (non-overlapping).
        assert_eq!(spans(Impl::Prefilter, "aa", "aaaa"), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn prefilter_skips_partial_literal_hits() {
        // "ab" literal occurs four times, but only the last is followed by 'c'.
        assert_eq!(
            spans(Impl::Prefilter, "abc", "ab ab abd abc"),
            vec![(10, 13)]
        );
    }
}
