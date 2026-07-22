// ABOUTME: Thompson-construction + Pike VM matcher (leftmost-first, greedy).
// ABOUTME: Linear time, no backtracking. Produces the SAME span set as `naive` and
// the oracle; it just does not explode on catastrophic-backtracking inputs. That is
// the headline demo result: identical output, exponentially faster.

//! Linear-time regex matching via a Pike VM.
//!
//! The AST is compiled once to a flat program of instructions (Thompson
//! construction). Matching simulates the program with a list of threads, ordered by
//! priority. The rules that make the output bit-identical to the backtracking
//! `naive` engine (leftmost-first, greedy) are:
//!
//! - threads carry their match `start`; a fresh start is seeded at each input
//!   position and appended to the *end* of the thread list, so threads from an
//!   earlier start keep higher priority (leftmost start wins);
//! - `best` is overwritten by every `Match` thread encountered, in priority order,
//!   so the highest-priority line's furthest end is kept (greedy);
//! - once a `Match` thread is reached at list-position `i` in a step, threads at
//!   positions `> i` are dropped for that step (a higher-priority match suppresses
//!   lower-priority extensions);
//! - once a match is found, no further starts are seeded; the surviving
//!   higher-priority threads run until they die, fixing the greedy end.

use crate::{Node, Regex, Span};

#[derive(Clone, Debug)]
enum Inst {
    /// Consume one byte in the class, then fall through to the next instruction.
    /// The class is boxed so a program is a compact array of small instructions
    /// (cache-friendly for the Pike VM's per-byte state walk).
    Byte(Box<crate::ByteClass>),
    /// Fork: the `pc1` thread has higher priority than `pc2`.
    Split(usize, usize),
    /// Unconditional jump.
    Jmp(usize),
    /// Accept the current match.
    Match,
    /// `^` — matches only at input position 0.
    StartAssert,
    /// `$` — matches only at the end of input.
    EndAssert,
}

#[derive(Clone, Copy, Debug)]
struct Thread {
    pc: usize,
    start: usize,
}

/// A compiled Thompson program. The instruction array is private; callers drive
/// it through [`pike_find`]. This wrapper exists so the prefilter can compile
/// once and verify many candidate starts without re-exposing the `Inst` type.
pub struct Program {
    insts: Vec<Inst>,
}

impl Program {
    /// Compile a pattern AST into a Thompson program ending in `Match`.
    pub fn new(ast: &Node) -> Self {
        let mut insts = Vec::new();
        emit(ast, &mut insts);
        insts.push(Inst::Match);
        Self { insts }
    }
}

fn set_l1(prog: &mut [Inst], idx: usize, target: usize) {
    if let Inst::Split(a, _) = &mut prog[idx] {
        *a = target;
    }
}

fn set_l2(prog: &mut [Inst], idx: usize, target: usize) {
    if let Inst::Split(_, b) = &mut prog[idx] {
        *b = target;
    }
}

fn emit(node: &Node, prog: &mut Vec<Inst>) {
    match node {
        Node::Empty => {}
        Node::Class(class) => prog.push(Inst::Byte(class.clone())),
        Node::StartAnchor => prog.push(Inst::StartAssert),
        Node::EndAnchor => prog.push(Inst::EndAssert),
        Node::Concat(parts) => {
            for part in parts {
                emit(part, prog);
            }
        }
        Node::Alt(branches) => {
            let mut splits = Vec::new();
            let mut jmps = Vec::new();
            for (i, branch) in branches.iter().enumerate() {
                let is_last = i + 1 == branches.len();
                if !is_last {
                    let sidx = prog.len();
                    prog.push(Inst::Split(0, 0));
                    let body = prog.len();
                    set_l1(prog, sidx, body); // body starts right after the split
                    splits.push(sidx);
                }
                emit(branch, prog);
                if !is_last {
                    let jidx = prog.len();
                    prog.push(Inst::Jmp(0));
                    jmps.push(jidx);
                    // This split's low-priority arm jumps past the `Jmp` to the next
                    // branch (or the next split, for chains longer than two).
                    let sidx = *splits.last().unwrap();
                    let next_branch = prog.len();
                    set_l2(prog, sidx, next_branch);
                }
            }
            let end = prog.len();
            for jidx in jmps {
                if let Inst::Jmp(target) = &mut prog[jidx] {
                    *target = end;
                }
            }
        }
        Node::Star(inner) => {
            let sidx = prog.len();
            prog.push(Inst::Split(0, 0));
            let body = prog.len();
            set_l1(prog, sidx, body); // body first (greedy)
            emit(inner, prog);
            prog.push(Inst::Jmp(sidx)); // loop back to the split
            let end = prog.len();
            set_l2(prog, sidx, end); // end falls through after the loop
        }
        Node::Plus(inner) => {
            let body_start = prog.len();
            emit(inner, prog);
            let sidx = prog.len();
            prog.push(Inst::Split(0, 0));
            set_l1(prog, sidx, body_start); // loop back for another rep (greedy)
            let end = prog.len();
            set_l2(prog, sidx, end); // end falls through
        }
        Node::Quest(inner) => {
            let sidx = prog.len();
            prog.push(Inst::Split(0, 0));
            let body = prog.len();
            set_l1(prog, sidx, body); // body first (greedy)
            emit(inner, prog);
            let end = prog.len();
            set_l2(prog, sidx, end); // skip is lower priority
        }
    }
}

/// Generation-stamped "have we added this pc yet this closure?" set. Avoids both
/// duplicate threads and infinite epsilon loops (e.g. `()*`).
struct Seen {
    stamps: Vec<u32>,
    gen: u32,
}

impl Seen {
    fn new(n: usize) -> Self {
        Self {
            stamps: vec![0; n],
            gen: 0,
        }
    }

    fn reset(&mut self) {
        self.gen = self.gen.wrapping_add(1);
        if self.gen == 0 {
            self.stamps.fill(0);
            self.gen = 1;
        }
    }

    /// Mark `pc`; return false if already marked this generation.
    fn mark(&mut self, pc: usize) -> bool {
        let seen = self.stamps[pc] == self.gen;
        if !seen {
            self.stamps[pc] = self.gen;
        }
        !seen
    }
}

/// Epsilon-close `pc` into `out`, expanding `Jmp`, `Split`, and asserts at `pos`.
/// `ready` threads (Byte / Match) are pushed in priority order.
fn add_thread(
    out: &mut Vec<Thread>,
    prog: &[Inst],
    pc: usize,
    start: usize,
    pos: usize,
    len: usize,
    seen: &mut Seen,
) {
    if !seen.mark(pc) {
        return;
    }
    match &prog[pc] {
        Inst::Jmp(target) => add_thread(out, prog, *target, start, pos, len, seen),
        Inst::Split(x, y) => {
            add_thread(out, prog, *x, start, pos, len, seen);
            add_thread(out, prog, *y, start, pos, len, seen);
        }
        Inst::StartAssert => {
            if pos == 0 {
                add_thread(out, prog, pc + 1, start, pos, len, seen);
            }
        }
        Inst::EndAssert => {
            if pos == len {
                add_thread(out, prog, pc + 1, start, pos, len, seen);
            }
        }
        _ => out.push(Thread { pc, start }),
    }
}

/// Find the leftmost-first greedy match at or after `scan`. Returns `(start, end)`.
///
/// When `unanchored` is false (used by the prefilter), only the initial start at
/// `scan` is seeded — no fresh starts at later positions. A match, if found, is
/// therefore anchored exactly at `scan`; this lets the prefilter verify a literal
/// hit without re-running the unanchored scan.
pub fn pike_find(
    prog: &Program,
    input: &[u8],
    scan: usize,
    unanchored: bool,
) -> Option<(usize, usize)> {
    let insts = prog.insts.as_slice();
    let len = input.len();
    if scan > len {
        return None;
    }
    let mut curr: Vec<Thread> = Vec::new();
    let mut next: Vec<Thread> = Vec::new();
    let mut curr_seen = Seen::new(insts.len());
    let mut next_seen = Seen::new(insts.len());

    let mut pos = scan;
    let mut best: Option<(usize, usize)> = None;

    // Seed the first start.
    curr_seen.reset();
    add_thread(&mut curr, insts, 0, scan, pos, len, &mut curr_seen);

    loop {
        next.clear();
        next_seen.reset();
        let mut matched_this_step = false;

        for &thread in &curr {
            if matched_this_step {
                break;
            }
            match &insts[thread.pc] {
                Inst::Byte(class) => {
                    if pos < len && class.matches(input[pos]) {
                        add_thread(
                            &mut next,
                            insts,
                            thread.pc + 1,
                            thread.start,
                            pos + 1,
                            len,
                            &mut next_seen,
                        );
                    }
                }
                Inst::Match => {
                    best = Some((thread.start, pos));
                    matched_this_step = true;
                }
                _ => {} // asserts/splits/jumps resolved by closure
            }
        }

        if pos == len {
            break;
        }
        // If a match is found but higher-priority threads are still alive, keep
        // extending to fix the greedy end. Otherwise we are done.
        if best.is_some() && next.is_empty() {
            break;
        }

        pos += 1;
        std::mem::swap(&mut curr, &mut next);
        std::mem::swap(&mut curr_seen, &mut next_seen);
        // Unanchored: seed a fresh (lowest-priority) start at the new position,
        // but only while no match has been found yet.
        if unanchored && best.is_none() {
            add_thread(&mut curr, insts, 0, pos, pos, len, &mut curr_seen);
        }
    }

    best
}

/// Find all non-overlapping leftmost-first matches of `re` in `input`.
pub fn find_all(re: &Regex, input: &[u8], out: &mut Vec<Span>) {
    let prog = Program::new(&re.ast);
    crate::scan_matches(input.len(), |scan| pike_find(&prog, input, scan, true), out);
}

#[cfg(test)]
mod tests {
    use crate::{Impl, Regex};

    fn t_spans(re: &str, input: &str) -> Vec<(usize, usize)> {
        let compiled = Regex::new(re).unwrap();
        let mut out = Vec::new();
        Impl::Thompson.find_all(&compiled, input.as_bytes(), &mut out);
        out.iter().map(|s| (s.start, s.end)).collect()
    }

    #[test]
    fn matches_naive_on_basics() {
        assert_eq!(t_spans("abc", "abcabc"), vec![(0, 3), (3, 6)]);
        assert_eq!(t_spans("a+", "aaaa"), vec![(0, 4)]);
        assert_eq!(t_spans("a*", "aaa"), vec![(0, 3)]);
        assert_eq!(t_spans("a*", "xx"), vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(t_spans("a|ab", "ab"), vec![(0, 1)]);
        assert_eq!(t_spans("ab|a", "ab"), vec![(0, 2)]);
        assert_eq!(t_spans("(a|b)*c", "aabbc"), vec![(0, 5)]);
        assert_eq!(t_spans("c$", "abc"), vec![(2, 3)]);
    }

    #[test]
    fn catastrophic_inputs_resolve_quickly() {
        // Same answer as naive (empty), in linear time.
        let input = "a".repeat(28) + "X";
        assert!(t_spans("(a+)+b", &input).is_empty());
        // Thompson handles a length naive would choke on.
        let big = format!("{}X", "a".repeat(2000));
        assert!(t_spans("(a+)+b", &big).is_empty());
    }

    /// Differential test: thompson must produce byte-identical spans to naive across
    /// a fixed set of (pattern, input) pairs covering the tricky semantic corners.
    #[test]
    fn thompson_equals_naive_on_semantic_corpus() {
        let pairs: &[(&str, &str)] = &[
            ("a*", ""),
            ("a*", "aaa"),
            ("a+", "aaaa"),
            ("a?b", "b"),
            ("a?b", "ab"),
            ("a|ab", "abab"),
            ("ab|a", "abab"),
            ("(a|b)*", "ababab"),
            ("(a|b)*c", "aabbc"),
            ("(ab)+", "abababx"),
            (".*", "abc"),
            ("[0-9]+", "x12y3"),
            (r"\w+", "  hi_1 "),
            ("^a", "ab"),
            ("a$", "ba"),
            ("", "ab"),
            ("(a*)*", "aaaa"),
            ("[a-z]*", "ABCabc"),
        ];
        for (pattern, input) in pairs {
            let re = Regex::new(pattern).unwrap();
            let mut naive_out = Vec::new();
            let mut thompson_out = Vec::new();
            Impl::Naive.find_all(&re, input.as_bytes(), &mut naive_out);
            Impl::Thompson.find_all(&re, input.as_bytes(), &mut thompson_out);
            assert_eq!(
                naive_out, thompson_out,
                "pattern={pattern:?} input={input:?}"
            );
        }
    }
}
