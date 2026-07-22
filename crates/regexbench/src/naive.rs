// ABOUTME: Reference backtracking matcher (leftmost-first, greedy).
// ABOUTME: Correct but exponential on catastrophic-backtracking inputs. It exists
// to be the slow baseline: it passes the same oracle as every other impl, so any
// speedup over it is provably performance, not a semantics change.

//! Recursive backtracking matcher.
//!
//! Continuation-passing style gives correct leftmost-first greedy semantics with
//! backtracking: each node receives the "rest of the pattern" as a continuation
//! `k`, and if a match path fails it tries the next alternative (e.g. a star
//! matching one fewer repetition). The first end position found in priority order
//! is the leftmost-first greedy match.

use crate::{Node, Regex, Span};

struct Matcher<'a> {
    input: &'a [u8],
}

impl<'a> Matcher<'a> {
    /// Try to match `node` at `pos`, calling `k` with each candidate end. Returns
    /// the first end (in leftmost-first greedy priority) for which `k` accepts.
    fn match_node(
        &self,
        node: &Node,
        pos: usize,
        k: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        match node {
            Node::Empty => k(pos),
            Node::Class(class) => {
                if pos < self.input.len() && class.matches(self.input[pos]) {
                    k(pos + 1)
                } else {
                    None
                }
            }
            Node::StartAnchor => {
                if pos == 0 {
                    k(pos)
                } else {
                    None
                }
            }
            Node::EndAnchor => {
                if pos == self.input.len() {
                    k(pos)
                } else {
                    None
                }
            }
            Node::Concat(parts) => self.match_seq(parts, 0, pos, k),
            Node::Alt(branches) => {
                for branch in branches {
                    if let Some(end) = self.match_node(branch, pos, k) {
                        return Some(end);
                    }
                }
                None
            }
            Node::Star(inner) => self.match_star(inner, pos, k),
            Node::Plus(inner) => {
                // One mandatory rep, then zero or more.
                let more = |p: usize| self.match_star(inner, p, k);
                self.match_node(inner, pos, &more)
            }
            Node::Quest(inner) => {
                // Greedy: prefer matching the optional part.
                self.match_node(inner, pos, k).or_else(|| k(pos))
            }
        }
    }

    /// Match a concatenation: node `idx` followed by the rest, then `k`.
    fn match_seq(
        &self,
        parts: &[Node],
        idx: usize,
        pos: usize,
        k: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        if idx == parts.len() {
            return k(pos);
        }
        let rest = |p: usize| self.match_seq(parts, idx + 1, p, k);
        self.match_node(&parts[idx], pos, &rest)
    }

    /// Greedy `*`: try one more repetition first, then stop. The zero-progress
    /// guard prevents infinite loops on nullable bodies like `(a*)*`.
    fn match_star(
        &self,
        inner: &Node,
        pos: usize,
        k: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        let more = |p: usize| {
            if p > pos {
                self.match_star(inner, p, k)
            } else {
                None
            }
        };
        self.match_node(inner, pos, &more).or_else(|| k(pos))
    }
}

/// Find all non-overlapping leftmost-first matches of `re` in `input`.
pub fn find_all(re: &Regex, input: &[u8], out: &mut Vec<Span>) {
    let matcher = Matcher { input };
    let len = input.len();
    crate::scan_matches(len, |scan| matcher.find_from(&re.ast, scan), out);
}

impl<'a> Matcher<'a> {
    /// Leftmost match at or after `scan`: try an anchored match at each start.
    fn find_from(&self, ast: &Node, scan: usize) -> Option<(usize, usize)> {
        let len = self.input.len();
        let mut start = scan;
        while start <= len {
            if let Some(end) = self.match_node(ast, start, &|p| Some(p)) {
                return Some((start, end));
            }
            start += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::Impl;

    fn spans(re: &str, input: &str) -> Vec<(usize, usize)> {
        let compiled = crate::Regex::new(re).unwrap();
        let mut out = Vec::new();
        Impl::Naive.find_all(&compiled, input.as_bytes(), &mut out);
        out.iter().map(|s| (s.start, s.end)).collect()
    }

    #[test]
    fn finds_non_overlapping_matches() {
        assert_eq!(spans("ab", "abxabab"), vec![(0, 2), (3, 5), (5, 7)]);
    }

    #[test]
    fn catastrophic_still_correct_on_short_input() {
        // Exponential in naive, but the answer is the empty set.
        let input = "a".repeat(20) + "X";
        assert!(spans("(a+)+b", &input).is_empty());
        // A matching variant still resolves to the leftmost-first span.
        assert_eq!(spans("a+", "aaaa"), vec![(0, 4)]);
    }
}
