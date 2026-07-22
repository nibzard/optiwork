// ABOUTME: Regex-engine demo subject for optiwork.
// ABOUTME: Defines the pattern AST + parser, the ByteClass primitive, Span, the
// candidate dispatch table, and the leftmost-first `find_all` contract that every
// impl must satisfy. All impls share one oracle (exact span-set equality), so a
// speed difference is provably performance, not semantics.

#![forbid(unsafe_code)]

//! A minimal regex subset built for benchmarking optimization candidates.
//!
//! Supported syntax (leftmost-first / Perl-style greedy semantics):
//! - literal bytes
//! - `.` any byte except newline
//! - `[abc]`, `[a-z]`, `[^...]` character classes
//! - `(...)` grouping, `|` alternation
//! - `*` `+` `?` greedy quantifiers
//! - `^` start-of-input, `$` end-of-input anchors
//! - `\d \w \s` and `\D \W \S`, plus backslash-escaped metacharacters
//!
//! The crate exposes several `--impl` matchers that MUST agree on the span set
//! produced by `find_all`. They differ only in time/memory on adversarial inputs.

pub mod corpus;
pub mod naive;
#[cfg(feature = "oracle")]
pub mod oracle;
pub mod prefilter;
#[cfg(feature = "oracle")]
pub mod sota;
pub mod thompson;

/// A half-open byte span `[start, end)` into the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// A predicate over a single byte, represented as a 256-bit membership table.
///
/// Every atom in the AST (literal, class, `.`, `\d`, …) lowers to a `ByteClass`,
/// which keeps the matcher implementations uniform: "does byte `b` match this
/// position?" is a single indexed array read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ByteClass(pub [bool; 256]);

impl ByteClass {
    pub const fn empty() -> Self {
        Self([false; 256])
    }

    /// `.` — any byte except newline (the conventional default).
    pub fn dot() -> Self {
        let mut bits = [true; 256];
        bits[b'\n' as usize] = false;
        Self(bits)
    }

    pub fn byte(b: u8) -> Self {
        let mut bits = [false; 256];
        bits[b as usize] = true;
        Self(bits)
    }

    pub fn range(lo: u8, hi: u8) -> Self {
        let mut bits = [false; 256];
        let mut i = lo as usize;
        while i <= hi as usize {
            bits[i] = true;
            i += 1;
        }
        Self(bits)
    }

    pub fn digit() -> Self {
        Self::range(b'0', b'9')
    }

    /// `\w` — ASCII word characters (alnum + underscore).
    pub fn word() -> Self {
        let mut bits = [false; 256];
        let mut i = 0u8;
        while i < 128 {
            if i.is_ascii_alphanumeric() || i == b'_' {
                bits[i as usize] = true;
            }
            i += 1;
        }
        Self(bits)
    }

    /// `\s` — ASCII whitespace.
    pub fn space() -> Self {
        let mut bits = [false; 256];
        for &c in &[b' ', b'\t', b'\n', b'\r', 0x0b, 0x0c] {
            bits[c as usize] = true;
        }
        Self(bits)
    }

    pub fn union(mut self, other: &ByteClass) -> Self {
        for i in 0..256 {
            self.0[i] |= other.0[i];
        }
        self
    }

    pub fn negate(mut self) -> Self {
        for i in 0..256 {
            self.0[i] = !self.0[i];
        }
        self
    }

    #[inline]
    pub fn matches(&self, b: u8) -> bool {
        self.0[b as usize]
    }

    /// If the class accepts exactly one byte, return it. Used by the prefilter to
    /// recognize literal characters at the start of a pattern.
    pub fn single_byte(&self) -> Option<u8> {
        let mut found: Option<u8> = None;
        for (byte, &set) in self.0.iter().enumerate() {
            if set {
                if found.is_some() {
                    return None;
                }
                found = Some(byte as u8);
            }
        }
        found
    }
}

/// Parsed pattern. Quantifiers are greedy; alternation is leftmost-first.
///
/// `Class` boxes the 256-byte membership table so the AST stays pointer-sized per
/// node (and, after compilation, the Pike VM program stays compact and
/// cache-friendly).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Empty,
    Class(Box<ByteClass>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Star(Box<Node>),
    Plus(Box<Node>),
    Quest(Box<Node>),
    StartAnchor,
    EndAnchor,
}

impl Node {
    /// Whether any match can span zero bytes (empty). Zero-width matches force the
    /// scanner to advance one byte after recording one, to avoid an infinite loop.
    pub fn nullable(&self) -> bool {
        match self {
            Node::Empty | Node::Star(_) | Node::Quest(_) => true,
            Node::StartAnchor | Node::EndAnchor => true,
            Node::Class(_) => false,
            Node::Plus(inner) => inner.nullable(),
            Node::Concat(parts) => parts.iter().all(Node::nullable),
            Node::Alt(parts) => parts.iter().any(Node::nullable),
        }
    }
}

/// A compiled pattern. Construction is always outside the timed region.
#[derive(Clone, Debug)]
pub struct Regex {
    pub ast: Node,
    pub source: String,
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Self, String> {
        let ast = parse(pattern)?;
        Ok(Self {
            ast,
            source: pattern.to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Parser (recursive descent over bytes).
// ---------------------------------------------------------------------------

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            bytes: input,
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, String> {
        Err(msg.into())
    }

    /// regex := alt
    fn parse_regex(&mut self) -> Result<Node, String> {
        let node = self.parse_alt()?;
        if self.pos != self.bytes.len() {
            let unexpected = match self.peek() {
                Some(b) => format!("byte `{}`", b as char),
                None => "end of pattern".to_string(),
            };
            return self.err(format!("unexpected {unexpected} after pattern"));
        }
        Ok(node)
    }

    /// alt := concat ('|' concat)*
    fn parse_alt(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some(b'|') {
            self.bump();
            branches.push(self.parse_concat()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap()
        } else {
            Node::Alt(branches)
        })
    }

    /// concat := repeat*  (until '|' or ')' or end)
    fn parse_concat(&mut self) -> Result<Node, String> {
        let mut parts = Vec::new();
        while let Some(b) = self.peek() {
            if b == b'|' || b == b')' {
                break;
            }
            parts.push(self.parse_repeat()?);
        }
        Ok(if parts.is_empty() {
            Node::Empty
        } else if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Node::Concat(parts)
        })
    }

    /// repeat := atom ('*' | '+' | '?')?
    fn parse_repeat(&mut self) -> Result<Node, String> {
        let atom = self.parse_atom()?;
        match self.peek() {
            Some(b'*') => {
                self.bump();
                Ok(Node::Star(Box::new(atom)))
            }
            Some(b'+') => {
                self.bump();
                Ok(Node::Plus(Box::new(atom)))
            }
            Some(b'?') => {
                self.bump();
                Ok(Node::Quest(Box::new(atom)))
            }
            _ => Ok(atom),
        }
    }

    /// atom := group | class | escape | anchor | literal
    fn parse_atom(&mut self) -> Result<Node, String> {
        match self.peek() {
            None => self.err("unexpected end of pattern"),
            Some(b'(') => {
                self.bump();
                let inner = self.parse_alt()?;
                if self.bump() != Some(b')') {
                    return self.err("unbalanced `(`");
                }
                Ok(inner)
            }
            Some(b'[') => self.parse_class(),
            Some(b'.') => {
                self.bump();
                Ok(Node::Class(Box::new(ByteClass::dot())))
            }
            Some(b'^') => {
                self.bump();
                Ok(Node::StartAnchor)
            }
            Some(b'$') => {
                self.bump();
                Ok(Node::EndAnchor)
            }
            Some(b'\\') => self.parse_escape(false).map(|c| Node::Class(Box::new(c))),
            Some(b'|') | Some(b')') => self.err("unexpected metacharacter"),
            Some(b'*') | Some(b'+') | Some(b'?') => self.err("quantifier with nothing to repeat"),
            Some(other) => {
                self.bump();
                Ok(Node::Class(Box::new(ByteClass::byte(other))))
            }
        }
    }

    /// Parse a `[...]` class: leading `^` negation, ranges `a-z`, `\d \w \s` and
    /// their negations. A `]` as the first (or first-after-`^`) character is literal.
    fn parse_class(&mut self) -> Result<Node, String> {
        self.bump(); // consume '['
        let mut class = ByteClass::empty();
        let negate = self.peek() == Some(b'^');
        if negate {
            self.bump();
        }
        let mut first = true;
        loop {
            let byte = match self.bump() {
                Some(b) => b,
                None => return self.err("unterminated character class"),
            };
            if byte == b']' && !first {
                break;
            }
            first = false;
            if byte == b'\\' {
                let escaped = self.parse_escape(true)?;
                class = class.union(&escaped);
                continue;
            }
            // Range, but a trailing '-' before ']' is a literal '-'.
            if self.peek() == Some(b'-')
                && self.bytes.get(self.pos + 1).copied() != Some(b']')
                && self.bytes.get(self.pos + 1).is_some()
            {
                self.bump(); // consume '-'
                let hi = match self.bump() {
                    Some(h) => h,
                    None => return self.err("unterminated character class"),
                };
                if byte > hi {
                    return self.err("invalid class range");
                }
                class = class.union(&ByteClass::range(byte, hi));
            } else {
                class = class.union(&ByteClass::byte(byte));
            }
        }
        if negate {
            class = class.negate();
        }
        Ok(Node::Class(Box::new(class)))
    }

    /// Parse a `\x` escape into a ByteClass. `in_class` rejects anchors, which are
    /// not allowed inside character classes.
    fn parse_escape(&mut self, in_class: bool) -> Result<ByteClass, String> {
        self.bump(); // consume backslash
        let byte = match self.bump() {
            Some(b) => b,
            None => return self.err("trailing backslash"),
        };
        Ok(match byte {
            b'd' => ByteClass::digit(),
            b'D' => ByteClass::digit().negate(),
            b'w' => ByteClass::word(),
            b'W' => ByteClass::word().negate(),
            b's' => ByteClass::space(),
            b'S' => ByteClass::space().negate(),
            b'n' => ByteClass::byte(b'\n'),
            b't' => ByteClass::byte(b'\t'),
            b'r' => ByteClass::byte(b'\r'),
            b'^' if !in_class => return self.err("`\\^` is not a valid escape"),
            other => ByteClass::byte(other),
        })
    }
}

pub fn parse(pattern: &str) -> Result<Node, String> {
    Parser::new(pattern.as_bytes()).parse_regex()
}

// ---------------------------------------------------------------------------
// Candidate dispatch.
// ---------------------------------------------------------------------------

/// Identifies a `find_all` implementation. Every variant must produce the exact
/// same span set as the oracle for all corpus pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Impl {
    Naive,
    Thompson,
    Prefilter,
}

impl Impl {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "naive" => Ok(Self::Naive),
            "thompson" => Ok(Self::Thompson),
            "prefilter" => Ok(Self::Prefilter),
            other => Err(format!(
                "unknown impl `{other}` (expected naive|thompson|prefilter)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Naive => "naive",
            Self::Thompson => "thompson",
            Self::Prefilter => "prefilter",
        }
    }

    /// Dispatch to the matcher. All impls share this contract.
    pub fn find_all(self, re: &Regex, input: &[u8], out: &mut Vec<Span>) {
        match self {
            Self::Naive => naive::find_all(re, input, out),
            Self::Thompson => thompson::find_all(re, input, out),
            Self::Prefilter => prefilter::find_all(re, input, out),
        }
    }
}

/// Drive a single-match `find_at(scan) -> Option<(start,end)>` routine across the
/// input, emitting non-overlapping leftmost-first spans. Implements the
/// empty-match rule shared with the `regex`-crate oracle: an empty match is
/// suppressed when it sits exactly at the end of the previous (non-empty) match,
/// and an empty match advances the scan by one to guarantee progress.
pub fn scan_matches(
    len: usize,
    mut find_at: impl FnMut(usize) -> Option<(usize, usize)>,
    out: &mut Vec<Span>,
) {
    let mut scan = 0usize;
    let mut last_end: Option<usize> = None;
    while scan <= len {
        let Some((start, end)) = find_at(scan) else {
            break;
        };
        if start == end && Some(start) == last_end {
            scan = start + 1;
            continue;
        }
        out.push(Span::new(start, end));
        last_end = Some(end);
        scan = if end > start { end } else { end + 1 };
    }
}

/// Convenience wrapper: find all non-overlapping leftmost-first matches via naive.
pub fn find_all(re: &Regex, input: &[u8], out: &mut Vec<Span>) {
    Impl::Naive.find_all(re, input, out);
}

/// Whether the pattern can match the empty string. Exposed so each impl's scanner
/// can decide whether to force-advance one byte after a zero-width match.
pub fn nullable(re: &Regex) -> bool {
    re.ast.nullable()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(re: &str, input: &str) -> Vec<(usize, usize)> {
        let compiled = Regex::new(re).unwrap();
        let mut out = Vec::new();
        Impl::Naive.find_all(&compiled, input.as_bytes(), &mut out);
        out.iter().map(|s| (s.start, s.end)).collect()
    }

    #[test]
    fn literal_matches() {
        assert_eq!(spans("abc", "abcabc"), vec![(0, 3), (3, 6)]);
        assert_eq!(spans("abc", "xxabcyyabc"), vec![(2, 5), (7, 10)]);
        assert!(spans("abc", "xyz").is_empty());
    }

    #[test]
    fn dot_and_anchors() {
        assert_eq!(spans(".", "ab"), vec![(0, 1), (1, 2)]);
        assert_eq!(spans("^a", "ba"), vec![]);
        assert_eq!(spans("^a", "ab"), vec![(0, 1)]);
        assert_eq!(spans("c$", "abc"), vec![(2, 3)]);
        assert_eq!(spans("c$", "cca"), vec![]);
    }

    #[test]
    fn classes_ranges_and_negation() {
        assert_eq!(spans("[0-9]+", "ab12cd3"), vec![(2, 4), (6, 7)]);
        assert_eq!(spans("[^0-9]+", "12ab3cd"), vec![(2, 4), (5, 7)]);
        assert_eq!(spans(r"\d+", "x42y7"), vec![(1, 3), (4, 5)]);
        assert_eq!(spans(r"\w+", "  hi_1 "), vec![(2, 6)]);
    }

    #[test]
    fn alternation_is_leftmost_first() {
        // leftmost-first: `a|ab` picks the first alternative; at position 1 ('b')
        // neither arm matches, so only one match results.
        assert_eq!(spans("a|ab", "ab"), vec![(0, 1)]);
        assert_eq!(spans("ab|a", "ab"), vec![(0, 2)]);
    }

    #[test]
    fn greedy_quantifiers_backtrack() {
        // No trailing empty match after a non-empty match (oracle rule).
        assert_eq!(spans("a*", "aaa"), vec![(0, 3)]);
        assert_eq!(spans("a+a", "aaaa"), vec![(0, 4)]);
        assert_eq!(spans("a?b", "b"), vec![(0, 1)]);
        assert_eq!(spans("a?b", "ab"), vec![(0, 2)]);
    }

    #[test]
    fn empty_matches_advance_one_byte() {
        // Zero-width matches at every position (incl. end), none adjacent to a
        // non-empty match.
        assert_eq!(spans("a*", "xx"), vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(spans("", "ab"), vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(spans("a*", "aaabaaa"), vec![(0, 3), (4, 7)]);
        assert_eq!(spans("a*", ""), vec![(0, 0)]);
    }

    #[test]
    fn group_and_nested_star() {
        assert_eq!(spans("(ab)+", "ababab"), vec![(0, 6)]);
        assert_eq!(spans("(a|b)*c", "aabbc"), vec![(0, 5)]);
    }

    #[test]
    fn parser_rejects_bad_syntax() {
        assert!(Regex::new("(").is_err());
        assert!(Regex::new(")").is_err());
        assert!(Regex::new("[abc").is_err());
        assert!(Regex::new("*").is_err());
        assert!(Regex::new("a(b").is_err());
        assert!(Regex::new("\\").is_err());
        assert!(Regex::new("[z-a]").is_err());
    }

    #[test]
    fn escapes_for_metacharacters() {
        assert_eq!(spans(r"\.", ".x."), vec![(0, 1), (2, 3)]);
        assert_eq!(spans(r"a\*b", "a*b"), vec![(0, 3)]);
    }
}
