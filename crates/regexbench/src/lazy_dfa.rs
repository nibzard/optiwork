// ABOUTME: Lazy/hybrid DFA matcher (leftmost-first, greedy), built on the Thompson
// ABOUTME: NFA. Literal-prefix patterns route to the SIMD memchr prefilter (so a large
// literal haystack is never scanned by a scalar automaton); non-literal patterns run a
// flat-table lazy DFA whose compiled transition cache is reused across find_all calls.
// The scan is byte-identical to the Pike VM (proven by the differential test below).

//! Leftmost-first/greedy matching via a lazy/hybrid DFA, with a literal fast-path.
//!
//! This matcher is a *meta-engine*: it keeps the existing `memchr` prefilter for
//! patterns with a leading literal prefix (the dominant-bytes case — a scalar automaton
//! over a large literal haystack cannot win the bytes/time throughput metric), and runs
//! a lazy DFA only for the non-literal pairs where the scalar Pike VM dominates *time*.
//!
//! **Why a DFA, and why flat.** `thompson::pike_find` re-runs the epsilon closure of
//! every live thread at every input byte. A lazy DFA computes each distinct
//! `(ordered NFA pc-set, byte)` transition once and caches it, so a step becomes an O(1)
//! flat-table lookup. The transition and provenance tables are flat `Vec`s indexed
//! `state*256 + byte` — no hashing, no per-byte allocation on the cached hot path. The
//! DFA is "lazy" (states built on demand, never the O(2^m) full build) and "hybrid"
//! (a state cap falls back to the byte-identical Pike VM if a pattern ever blows up the
//! table — a memory bound, never a correctness one).
//!
//! **Leftmost-first correctness — the hard part.** A DFA state (a pc-set) is
//! position-independent, but unanchored leftmost-first matching must recover *where* a
//! match started. We carry a parallel `starts` list through the scan: each cached
//! transition also stores its *provenance* (`src[k]` = which source-state pc produced
//! the k-th pc of the destination state), so `next.starts` is rebuilt as
//! `curr.starts[src[k]]`. The thread ordering, match-suppression (a `Match` reached at
//! list-position `i` drops lower-priority threads for that step), and greedy-end rules
//! are identical to `pike_find` — the differential test below proves byte-identity.
//!
//! **Cross-call caching.** The compiled DFA is memoized per pattern (thread-local, keyed
//! on the source string) so the build cost is paid once and amortized over every session.
//!
//! **Anchored patterns** (`^`/`$`) are position-dependent and routed to
//! `thompson::find_all`.

use crate::thompson::{Inst, Program};
use crate::{Node, Regex, Span};
use std::cell::RefCell;
use std::collections::HashMap;

/// Sentinel for an uncomputed flat-table transition or provenance offset.
const UNCOMPUTED: u32 = u32::MAX;
/// Above this many live DFA states we stop caching and defer to the Pike VM. Our
/// patterns produce a handful of states; the cap is a safety valve for adversarial
/// patterns. At 256 u32 per state this bounds the transition table to ~10 MiB.
const STATE_CAP: usize = 10_000;

thread_local! {
    /// One compiled DFA per distinct pattern source, reused across `find_all` calls.
    static DFA_CACHE: RefCell<HashMap<String, Dfa>> = RefCell::new(HashMap::new());
}

/// True if the pattern AST contains a position-dependent assertion (`^` / `$`). Such
/// patterns are matched by the position-aware Pike VM instead of this matcher. Checked
/// on the AST so the cache-hit path never builds a `Program`.
fn has_anchors_ast(node: &Node) -> bool {
    match node {
        Node::StartAnchor | Node::EndAnchor => true,
        Node::Empty | Node::Class(_) => false,
        Node::Concat(parts) | Node::Alt(parts) => parts.iter().any(has_anchors_ast),
        Node::Star(inner) | Node::Plus(inner) | Node::Quest(inner) => has_anchors_ast(inner),
    }
}

/// Epsilon-close `pc`: expand `Jmp`/`Split`, push ready instructions (`Byte`/`Match`)
/// in priority order. `Split(x, y)` visits `x` first (higher priority). Asserts never
/// appear here (anchored programs are routed to the Pike VM beforehand).
fn build_closure(prog: &[Inst], pc: usize, out: &mut Vec<usize>, seen: &mut [bool]) {
    if seen[pc] {
        return;
    }
    seen[pc] = true;
    match &prog[pc] {
        Inst::Jmp(target) => build_closure(prog, *target, out, seen),
        Inst::Split(x, y) => {
            build_closure(prog, *x, out, seen);
            build_closure(prog, *y, out, seen);
        }
        _ => out.push(pc), // Byte / Match ready; asserts unreachable here.
    }
}

/// A DFA state: an ordered, de-duplicated list of ready program counters, the index of
/// the first `Match` (for leftmost-first match-suppression) if any, and the pc count
/// (also the provenance length for transitions into this state).
struct State {
    pcs: Vec<usize>,
    first_match: Option<usize>,
    len: u32,
}

/// A lazy/hybrid DFA for one pattern, built once and cached per `find_all`. All hot-path
/// state is flat-indexed and scratch buffers are reused, so a cached step allocates
/// nothing.
struct Dfa {
    prog: Vec<Inst>,
    /// Ready pcs of the program-start closure (the unanchored seed), lowest priority.
    start_pcs: Vec<usize>,
    start_state: u32,
    states: Vec<State>,
    /// pc-list -> state id, for de-duplication when interning a new state.
    key: HashMap<Vec<usize>, u32>,
    /// Flat next-state table: `trans[state*256 + byte]` = destination state id (or
    /// `UNCOMPUTED`). Id 0 is the dead/empty state.
    trans: Vec<u32>,
    /// Flat provenance: for the transition `(state, byte)` whose offset is
    /// `prov_off[state*256+byte]`, `prov[off + k]` is the index (in `state`'s pcs) of
    /// the source pc whose closure produced the destination's k-th pc.
    prov: Vec<u16>,
    prov_off: Vec<u32>,
    /// Memoized unanchored seed-merge: `seed_merged[state]` is the state id formed by
    /// appending the start closure to `state` (de-duplicated); `seed_appended[state]` is
    /// how many seed pcs were added. The merged pcs depend only on `state`, not on the
    /// scan position, so this is computed once per state — not per byte.
    seed_merged: Vec<u32>,
    seed_appended: Vec<u16>,
    /// Pre-computed epsilon closure of every pc (built once; the program is small).
    closure_cache: Vec<Vec<usize>>,
    /// Scratch membership set over `prog.len()`, reused to avoid per-step allocation.
    present: Vec<bool>,
    /// Set when `STATE_CAP` is hit; the caller discards results and defers to Pike VM.
    overflowed: bool,
}

impl Dfa {
    fn build(prog: Vec<Inst>) -> Self {
        let n = prog.len();
        // Pre-compute every pc's closure once (build-time only; the program is small).
        let mut closure_cache = vec![Vec::new(); n];
        let mut seen = vec![false; n];
        for (pc, slot) in closure_cache.iter_mut().enumerate() {
            let mut out = Vec::new();
            build_closure(&prog, pc, &mut out, &mut seen);
            seen.fill(false);
            *slot = out;
        }
        let mut dfa = Self {
            prog,
            start_pcs: Vec::new(),
            start_state: 0,
            states: Vec::new(),
            key: HashMap::new(),
            trans: Vec::new(),
            prov: Vec::new(),
            prov_off: Vec::new(),
            seed_merged: Vec::new(),
            seed_appended: Vec::new(),
            closure_cache,
            present: vec![false; n],
            overflowed: false,
        };
        // Id 0 is the dead/empty state: every byte transitions to itself.
        let _dead = dfa.intern(Vec::new());
        debug_assert_eq!(_dead, 0);
        let start = dfa.closure_cache[0].clone();
        dfa.start_pcs = start.clone();
        dfa.start_state = dfa.intern(start);
        dfa
    }

    /// De-duplicate a pc-list by id, creating a new state on first sight. Records the
    /// first `Match` position and grows the flat tables by one 256-wide row. Honours
    /// `STATE_CAP` by poisoning the DFA (caller falls back to the Pike VM).
    fn intern(&mut self, pcs: Vec<usize>) -> u32 {
        if let Some(&id) = self.key.get(&pcs) {
            return id;
        }
        if self.states.len() >= STATE_CAP {
            self.overflowed = true;
            return 0; // dead state; results are discarded by the caller.
        }
        let first_match = pcs
            .iter()
            .position(|&pc| matches!(self.prog[pc], Inst::Match));
        let len = pcs.len() as u32;
        let id = self.states.len() as u32;
        self.states.push(State {
            pcs: pcs.clone(),
            first_match,
            len,
        });
        self.key.insert(pcs, id);
        // Grow the flat tables by one 256-wide row, all uncomputed.
        self.trans.extend(std::iter::repeat_n(UNCOMPUTED, 256));
        self.prov_off.extend(std::iter::repeat_n(UNCOMPUTED, 256));
        self.seed_merged.push(UNCOMPUTED);
        self.seed_appended.push(0);
        id
    }

    /// Compute and cache the transition from `id` on `byte` (match-truncated). Walks the
    /// source pcs in priority order; a matching `Byte` contributes its closure; the first
    /// `Match` stops the walk (leftmost-first suppression). Runs only on a cache miss.
    fn compute_step(&mut self, id: u32, byte: u8) {
        let slot = id as usize * 256 + byte as usize;
        let src_pcs = self.states[id as usize].pcs.clone();
        for &p in &src_pcs {
            self.present[p] = false;
        }
        let mut next_pcs: Vec<usize> = Vec::new();
        let mut src: Vec<u16> = Vec::new();
        for (i, &pc) in src_pcs.iter().enumerate() {
            match &self.prog[pc] {
                Inst::Byte(class) => {
                    if class.matches(byte) {
                        // Closure results are pre-computed; clone releases the borrow so
                        // `intern` below can mutate (cache-miss path only).
                        let closure = self.closure_cache[pc + 1].clone();
                        for ready in closure {
                            if !self.present[ready] {
                                self.present[ready] = true;
                                next_pcs.push(ready);
                                src.push(i as u16);
                            }
                        }
                    }
                }
                Inst::Match => break, // suppress lower-priority threads this step
                _ => {}               // splits/jumps/asserts resolved by closure
            }
        }
        for &p in &next_pcs {
            self.present[p] = false;
        }
        let next = self.intern(next_pcs);
        if self.overflowed {
            return;
        }
        let off = self.prov.len() as u32;
        self.prov.extend_from_slice(&src);
        self.trans[slot] = next;
        self.prov_off[slot] = off;
    }

    /// Advance `curr_starts` through the cached transition on `byte`, writing the next
    /// starts into `next_starts` (cleared first). Returns the destination state id. No
    /// allocation on the cached path.
    fn transition_into(
        &mut self,
        curr_id: u32,
        curr_starts: &[usize],
        byte: u8,
        next_starts: &mut Vec<usize>,
    ) -> u32 {
        let slot = curr_id as usize * 256 + byte as usize;
        if self.trans[slot] == UNCOMPUTED {
            self.compute_step(curr_id, byte);
        }
        if self.overflowed {
            return 0;
        }
        let next_id = self.trans[slot];
        let off = self.prov_off[slot] as usize;
        let len = self.states[next_id as usize].len as usize;
        next_starts.clear();
        next_starts.reserve(len);
        for k in 0..len {
            let s = self.prov[off + k] as usize;
            // A dead/empty destination has no provenance; guard the index.
            next_starts.push(if s < curr_starts.len() {
                curr_starts[s]
            } else {
                0
            });
        }
        next_id
    }

    /// Append the program-start closure (seeded at `pos`, lowest priority) to the
    /// unseeded transition, de-duplicating against pcs already present. Writes the
    /// combined starts into `out_starts` and returns the combined state id.
    ///
    /// The combined *pcs* depend only on `next_id` (the start closure is constant), so
    /// the merged state is memoized per `next_id` and computed once — only the appended
    /// `starts` (the new position `pos`) change per byte.
    fn merge_seed_into(
        &mut self,
        next_id: u32,
        next_starts: &[usize],
        pos: usize,
        out_starts: &mut Vec<usize>,
    ) -> u32 {
        let idx = next_id as usize;
        if self.seed_merged[idx] == UNCOMPUTED {
            let next_pcs = self.states[idx].pcs.clone();
            for &p in &next_pcs {
                self.present[p] = false;
            }
            let mut combined: Vec<usize> = next_pcs.clone();
            for &p in &combined {
                self.present[p] = true;
            }
            let mut appended = 0u16;
            for &p in &self.start_pcs {
                if !self.present[p] {
                    self.present[p] = true;
                    combined.push(p);
                    appended += 1;
                }
            }
            for &p in &combined {
                self.present[p] = false;
            }
            let id = self.intern(combined);
            self.seed_merged[idx] = id;
            self.seed_appended[idx] = appended;
        }
        let merged_id = self.seed_merged[idx];
        let appended = self.seed_appended[idx] as usize;
        out_starts.clear();
        out_starts.extend_from_slice(next_starts);
        out_starts.resize(next_starts.len() + appended, pos);
        merged_id
    }

    /// Find the leftmost-first greedy match at or after `scan`. Mirrors `pike_find`:
    /// seed at `scan`, detect a match via `first_match`, keep extending the greedy end
    /// while higher-priority threads survive, seed fresh (lowest-priority) starts only
    /// while no match is found yet.
    fn cached_find(&mut self, input: &[u8], scan: usize) -> Option<(usize, usize)> {
        let len = input.len();
        if scan > len {
            return None;
        }
        let start_state = self.start_state;
        let start_count = self.start_pcs.len();
        let mut curr_id = start_state;
        let mut curr_starts: Vec<usize> = vec![scan; start_count];
        let mut next_starts: Vec<usize> = Vec::new();
        let mut pos = scan;
        let mut best: Option<(usize, usize)> = None;

        loop {
            // A `Match` thread in the current state records/extends the best match.
            if let Some(idx) = self.states[curr_id as usize].first_match {
                if idx < curr_starts.len() {
                    best = Some((curr_starts[idx], pos));
                }
            }
            if pos == len {
                break;
            }
            let next_id = self.transition_into(curr_id, &curr_starts, input[pos], &mut next_starts);
            if self.overflowed {
                return None;
            }
            // Greedy end: a match is found and no higher-priority thread survives.
            if best.is_some() && self.states[next_id as usize].pcs.is_empty() {
                break;
            }
            pos += 1;
            if best.is_none() {
                // Unanchored: seed a fresh lowest-priority start at the new position.
                curr_id = self.merge_seed_into(next_id, &next_starts, pos, &mut curr_starts);
                if self.overflowed {
                    return None;
                }
            } else {
                curr_id = next_id;
                std::mem::swap(&mut curr_starts, &mut next_starts);
            }
        }

        best
    }
}

/// Find all non-overlapping leftmost-first matches of `re` in `input`.
///
/// Literal-prefix patterns (the dominant-bytes case) route to the `memchr` prefilter so a
/// large literal haystack is never handed to a scalar automaton. Anchored patterns route
/// to the Pike VM (position-dependent assertions can't be cached). Everything else runs
/// the cached lazy DFA.
pub fn find_all(re: &Regex, input: &[u8], out: &mut Vec<Span>) {
    if let Some(prefix) = crate::prefilter::leading_literal_prefix(&re.ast) {
        if prefix.len() >= 2 {
            return crate::prefilter::find_all(re, input, out);
        }
    }
    if has_anchors_ast(&re.ast) {
        // Position-dependent assertions can't be cached: defer to the Pike VM.
        crate::thompson::find_all(re, input, out);
        return;
    }
    DFA_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        // Build only on the first sight of this pattern; the hot cache-hit path is two
        // hash lookups and no allocation (no `source` clone, no `Program` rebuild).
        if !cache.contains_key(re.source.as_str()) {
            let prog = Program::new(&re.ast);
            cache.insert(re.source.clone(), Dfa::build(prog.insts().to_vec()));
        }
        let dfa = cache.get_mut(re.source.as_str()).expect("just inserted");
        crate::scan_matches(input.len(), |scan| dfa.cached_find(input, scan), out);
        if dfa.overflowed {
            // State cap hit: discard and run the byte-identical Pike VM instead.
            out.clear();
            crate::thompson::find_all(re, input, out);
        }
    });
}

#[cfg(test)]
mod tests {
    use crate::{corpus, Impl, Regex};

    fn dfa_spans(re: &str, input: &str) -> Vec<(usize, usize)> {
        let compiled = Regex::new(re).unwrap();
        let mut out = Vec::new();
        Impl::LazyDfa.find_all(&compiled, input.as_bytes(), &mut out);
        out.iter().map(|s| (s.start, s.end)).collect()
    }

    #[test]
    fn lazy_dfa_matches_thompson_on_basics() {
        assert_eq!(dfa_spans("abc", "abcabc"), vec![(0, 3), (3, 6)]);
        assert_eq!(dfa_spans("a+", "aaaa"), vec![(0, 4)]);
        assert_eq!(dfa_spans("a*", "aaa"), vec![(0, 3)]);
        assert_eq!(dfa_spans("a*", "xx"), vec![(0, 0), (1, 1), (2, 2)]);
        // The leftmost-first crux: `a|ab` picks the first alternative.
        assert_eq!(dfa_spans("a|ab", "ab"), vec![(0, 1)]);
        assert_eq!(dfa_spans("ab|a", "ab"), vec![(0, 2)]);
        assert_eq!(dfa_spans("(a|b)*c", "aabbc"), vec![(0, 5)]);
        // Cross-call caching must not leak state between patterns/inputs.
        assert_eq!(dfa_spans("a+", "aa"), vec![(0, 2)]);
        assert_eq!(dfa_spans("ab|a", "aba"), vec![(0, 2), (2, 3)]);
    }

    #[test]
    fn lazy_dfa_literal_route_matches_naive() {
        // Literal-prefix patterns route to the memchr prefilter; still oracle-correct.
        assert_eq!(
            dfa_spans("needle", "xneedle neex needle y"),
            vec![(1, 7), (13, 19)]
        );
        assert_eq!(dfa_spans("abc", "ab ab abd abc"), vec![(10, 13)]);
    }

    #[test]
    fn lazy_dfa_routes_anchors_to_pike_vm() {
        // Anchored patterns defer to thompson; results must still match the oracle.
        assert_eq!(dfa_spans("^a", "ab"), vec![(0, 1)]);
        assert_eq!(dfa_spans("c$", "abc"), vec![(2, 3)]);
    }

    #[test]
    fn lazy_dfa_matches_thompson_on_corpus_pairs() {
        for path in ["corpora/main.bin", "corpora/pathological.bin"] {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let Ok(pairs) = corpus::deserialize_corpus(&bytes) else {
                continue;
            };
            for pair in &pairs {
                let Ok(re) = Regex::new(&pair.pattern) else {
                    continue;
                };
                let mut a = Vec::new();
                let mut b = Vec::new();
                Impl::Thompson.find_all(&re, &pair.input, &mut a);
                Impl::LazyDfa.find_all(&re, &pair.input, &mut b);
                assert_eq!(
                    a, b,
                    "corpus={path} pattern={:?} input={:?}",
                    pair.pattern, pair.input
                );
            }
        }
    }

    /// SplitMix64 — a self-contained PRNG so the property test adds no dependency
    /// (keeps `cargo test --locked` green and respects the oracle boundary).
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
    }

    /// A tiny pattern grammar (no anchors, so it exercises the DFA core, not the
    /// fallback). Builds syntactically-valid leftmost-first patterns.
    fn gen_pattern(r: &mut Rng, depth: usize) -> String {
        let atom = |r: &mut Rng| -> String {
            match r.below(6) {
                0 => "a".to_owned(),
                1 => "b".to_owned(),
                2 => ".".to_owned(),
                3 => r#"\d"#.to_owned(),
                4 => "[ab]".to_owned(),
                _ => "(a|b)".to_owned(),
            }
        };
        let mut s = String::new();
        let terms = 1 + r.below(3);
        for _ in 0..terms {
            let unit = if depth < 3 && r.below(4) == 0 {
                format!("({})", gen_pattern(r, depth + 1))
            } else {
                atom(r)
            };
            s.push_str(&match r.below(5) {
                0 => format!("{unit}*"),
                1 => format!("{unit}+"),
                2 => format!("{unit}?"),
                _ => unit,
            });
        }
        if r.below(3) == 0 {
            format!("{}|{}", s, atom(r))
        } else {
            s
        }
    }

    fn gen_input(r: &mut Rng) -> String {
        let alphabet = b"ab012";
        let n = r.below(12);
        (0..n)
            .map(|_| alphabet[r.below(alphabet.len())] as char)
            .collect()
    }

    /// Differential property test: `LazyDfa` must equal `Thompson` (itself
    /// byte-identical to the oracle) on fixed semantic corners and ~20k randomized
    /// (pattern, input) pairs. Any mismatch fails before any timing.
    #[test]
    fn lazy_dfa_equals_thompson_differentially() {
        let corners: &[(&str, &str)] = &[
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
            ("", "ab"),
            ("(a*)*", "aaaa"),
            ("(a+)+b", "aaaab"),
            ("(a+)+b", "aaaX"),
            ("[a-z]*", "ABCabc"),
            ("a*a*a*", "aaa"),
        ];

        let check = |pattern: &str, input: &str| {
            let Ok(re) = Regex::new(pattern) else {
                return;
            };
            let mut a = Vec::new();
            let mut b = Vec::new();
            Impl::Thompson.find_all(&re, input.as_bytes(), &mut a);
            Impl::LazyDfa.find_all(&re, input.as_bytes(), &mut b);
            assert_eq!(a, b, "pattern={pattern:?} input={input:?}");
        };

        for (p, i) in corners {
            check(p, i);
        }

        let mut rng = Rng::new(0xC0FFEE);
        for _ in 0..20_000 {
            let pattern = gen_pattern(&mut rng, 0);
            let input = gen_input(&mut rng);
            if Regex::new(&pattern).is_ok() {
                check(&pattern, &input);
            }
        }
    }
}
