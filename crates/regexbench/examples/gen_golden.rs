// ABOUTME: Offline generator for regexbench corpora and golden vectors.
// ABOUTME: Builds seven adversarial categories, splits them into main/pathological
// ABOUTME: corpora, and writes matching golden files via the `regex`-crate oracle.
// ABOUTME: Never runs in the timed path; rebuild only when the corpus changes.

use std::fs;
use std::path::{Path, PathBuf};

use regexbench::corpus::{
    corpus_bytes, deserialize_corpus, serialize_corpus, serialize_golden, Pair,
};
use regexbench::oracle;

fn pair(pattern: &str, input: impl Into<Vec<u8>>) -> Pair {
    Pair {
        pattern: pattern.to_owned(),
        input: input.into(),
    }
}

/// Category 1: empty / zero-width matches.
fn cat_empty() -> Vec<Pair> {
    vec![
        pair("a*", ""),
        pair("a*", "xxx"),
        pair("", "ab"),
        pair("a*", "aaa"),
        pair("b?", "bbb"),
        pair("(a*)*", "aaaa"),
        pair("a*b*", "xx"),
    ]
}

/// Category 2: anchors.
fn cat_anchors() -> Vec<Pair> {
    vec![
        pair("^abc", "abcdef"),
        pair("^abc", "xabcdef"),
        pair("c$", "abc"),
        pair("c$", "abca"),
        pair("^a*c$", "aaac"),
        pair("^a*c$", "aaad"),
        pair("^$", ""),
        pair("^[a-z]+$", "hello"),
        pair("^[a-z]+$", "hi!"),
    ]
}

/// Category 3: anchors + alternation.
fn cat_anchors_alt() -> Vec<Pair> {
    vec![
        pair("^(cat|dog)$", "cat"),
        pair("^(cat|dog)$", "bird"),
        pair("^(a|b)+c$", "ababc"),
        pair("^(a|b)+c$", "abac"),
        pair("^(foo|bar|baz)$", "baz"),
        pair("^(foo|bar|baz)$", "qux"),
        pair("(ing|ed)$", "walked"),
        pair("(ing|ed)$", "running"),
    ]
}

/// Category 4: greedy quantifiers and backtracking.
fn cat_greedy() -> Vec<Pair> {
    vec![
        pair("a+", "aaaa"),
        pair("a*b", "aaab"),
        pair("(ab)+", "abababx"),
        pair("a?a?a?b", "aab"),
        pair("a?a?a?b", "bbb"),
        pair("a.*b", "aXbYaZb"),
        pair("x+y+z", "xxxyyz"),
        pair("[0-9]+\\.[0-9]+", "pi=3.14159"),
        pair("a|ab", "abab"),
        pair("ab|a", "abab"),
        pair("a+a", "aaaa"),
        pair("(a|b)*c", "aabbc"),
    ]
}

/// Category 5: multibyte UTF-8 (byte-offset correctness).
fn cat_multibyte() -> Vec<Pair> {
    vec![
        pair(".", "café"),
        pair("é", "café"),
        pair("[^a]", "café"),
        pair(".+", "αβγ"),
        pair("é+", "cafécafé"),
        pair("c.f.", "café"),
    ]
}

/// Category 6: large haystack with sparse literal matches (throughput on big input).
fn cat_large() -> Vec<Pair> {
    let mut haystack = String::with_capacity(8192 + 48);
    for _ in 0..(8192 / 4) {
        haystack.push_str("xxxx");
    }
    // Insert a handful of needles at known offsets.
    let needles = ["needle", "needle", "needle", "needle"];
    let positions = [500, 2100, 4300, 7000];
    let mut bytes: Vec<u8> = haystack.into_bytes();
    for (i, &p) in positions.iter().enumerate() {
        let nb = needles[i].as_bytes();
        bytes[p..p + nb.len()].copy_from_slice(nb);
    }
    vec![pair("needle", bytes)]
}

/// Category 7: catastrophic-backtracking inputs (the headline naive→thompson case).
fn cat_pathological() -> Vec<Pair> {
    let a24 = "a".repeat(24);
    let a20 = "a".repeat(20);
    let a16 = "a".repeat(16);
    vec![
        // No-match catastrophic: naive is exponential, thompson linear.
        pair("(a+)+b", format!("{a24}X")),
        pair("(a+)+$", format!("{a24}!")),
        pair("(a|b)*$", format!("{a20}X")),
        pair("(a+)+b", format!("{a20}!")),
        // Matching variants (still exercised by both engines).
        pair("(a|b)*a(b|a)", format!("{a16}a")),
        pair("(a|b)*a(b|a)", format!("{a16}b")),
        pair("(a+)+", a24.clone()),
    ]
}

fn build_all() -> (Vec<Pair>, Vec<Pair>) {
    let mut main = Vec::new();
    main.extend(cat_empty());
    main.extend(cat_anchors());
    main.extend(cat_anchors_alt());
    main.extend(cat_greedy());
    main.extend(cat_multibyte());
    main.extend(cat_large());
    let pathological = cat_pathological();
    (main, pathological)
}

fn write_corpus_and_golden(dir: &Path, name: &str, pairs: &[Pair]) -> Result<(), String> {
    let corpus_path = dir.join(format!("{name}.bin"));
    let golden_path = dir.join(format!("{name}.golden"));
    fs::write(&corpus_path, serialize_corpus(pairs))
        .map_err(|e| format!("could not write {}: {e}", corpus_path.display()))?;
    let golden = oracle::golden_for(pairs)?;
    fs::write(&golden_path, serialize_golden(&golden))
        .map_err(|e| format!("could not write {}: {e}", golden_path.display()))?;
    println!(
        "{}: {} pairs, {} bytes",
        name,
        pairs.len(),
        corpus_bytes(pairs)
    );
    Ok(())
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(args.first().map(String::as_str).unwrap_or("corpora"));
    fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    let (main, pathological) = build_all();
    write_corpus_and_golden(&dir, "main", &main)?;
    write_corpus_and_golden(&dir, "pathological", &pathological)?;

    // Self-check: reload each corpus and confirm the written golden still matches a
    // freshly computed one (catches serialization bugs before any timing run).
    for name in ["main", "pathological"] {
        let reloaded = deserialize_corpus(&fs::read(dir.join(format!("{name}.bin"))).unwrap())
            .map_err(|e| format!("reload {name}: {e}"))?;
        let recheck = oracle::golden_for(&reloaded)?;
        let golden = regexbench::corpus::deserialize_golden(
            &fs::read(dir.join(format!("{name}.golden"))).unwrap(),
        )
        .map_err(|e| format!("reload golden {name}: {e}"))?;
        for (written, fresh) in golden.iter().zip(recheck.iter()) {
            if written.pattern != fresh.pattern
                || written.input != fresh.input
                || written.spans != fresh.spans
            {
                return Err(format!("golden self-check failed for {name}"));
            }
        }
        println!("{name}: golden self-check ok ({} pairs)", golden.len());
    }
    Ok(())
}
