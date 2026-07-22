// ABOUTME: Length-prefixed binary corpus + golden-vector serialization for
// ABOUTME: regexbench. A corpus is a list of (pattern, input) pairs; a golden file
// ABOUTME: adds the oracle's expected match spans per pair for the equivalence gate.

//! Corpus and golden-vector on-disk formats.
//!
//! Both formats are little-endian length-prefixed binary, with a versioned magic
//! header so a format change cannot silently corrupt a campaign.

use crate::Span;

pub const CORPUS_MAGIC: &[u8] = b"optiwork-corpus-v1\n";
pub const GOLDEN_MAGIC: &[u8] = b"optiwork-golden-v1\n";

/// One `(pattern, haystack)` benchmark pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub pattern: String,
    pub input: Vec<u8>,
}

/// A pair together with the oracle's expected non-overlapping leftmost-first spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoldenPair {
    pub pattern: String,
    pub input: Vec<u8>,
    pub spans: Vec<Span>,
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32, String> {
    if *pos + 4 > bytes.len() {
        return Err("unexpected end of file while reading u32".to_owned());
    }
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[*pos..*pos + 4]);
    *pos += 4;
    Ok(u32::from_le_bytes(buf))
}

fn read_bytes(bytes: &[u8], pos: &mut usize) -> Result<Vec<u8>, String> {
    let len = read_u32(bytes, pos)? as usize;
    if *pos + len > bytes.len() {
        return Err("unexpected end of file while reading blob".to_owned());
    }
    let blob = bytes[*pos..*pos + len].to_vec();
    *pos += len;
    Ok(blob)
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_blob(out: &mut Vec<u8>, blob: &[u8]) {
    write_u32(out, blob.len() as u32);
    out.extend_from_slice(blob);
}

/// Serialize a corpus (pairs only).
pub fn serialize_corpus(pairs: &[Pair]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(CORPUS_MAGIC);
    write_u32(&mut out, pairs.len() as u32);
    for pair in pairs {
        write_blob(&mut out, pair.pattern.as_bytes());
        write_blob(&mut out, &pair.input);
    }
    out
}

/// Deserialize a corpus.
pub fn deserialize_corpus(bytes: &[u8]) -> Result<Vec<Pair>, String> {
    if bytes.len() < CORPUS_MAGIC.len() || &bytes[..CORPUS_MAGIC.len()] != CORPUS_MAGIC {
        return Err("not an optiwork-corpus-v1 file".to_owned());
    }
    let mut pos = CORPUS_MAGIC.len();
    let count = read_u32(bytes, &mut pos)? as usize;
    let mut pairs = Vec::with_capacity(count);
    for _ in 0..count {
        let pattern = String::from_utf8(read_bytes(bytes, &mut pos)?)
            .map_err(|_| "pattern is not UTF-8".to_owned())?;
        let input = read_bytes(bytes, &mut pos)?;
        pairs.push(Pair { pattern, input });
    }
    Ok(pairs)
}

/// Serialize golden vectors (pairs + expected spans).
pub fn serialize_golden(pairs: &[GoldenPair]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(GOLDEN_MAGIC);
    write_u32(&mut out, pairs.len() as u32);
    for pair in pairs {
        write_blob(&mut out, pair.pattern.as_bytes());
        write_blob(&mut out, &pair.input);
        write_u32(&mut out, pair.spans.len() as u32);
        for span in &pair.spans {
            write_u32(&mut out, span.start as u32);
            write_u32(&mut out, span.end as u32);
        }
    }
    out
}

/// Deserialize golden vectors.
pub fn deserialize_golden(bytes: &[u8]) -> Result<Vec<GoldenPair>, String> {
    if bytes.len() < GOLDEN_MAGIC.len() || &bytes[..GOLDEN_MAGIC.len()] != GOLDEN_MAGIC {
        return Err("not an optiwork-golden-v1 file".to_owned());
    }
    let mut pos = GOLDEN_MAGIC.len();
    let count = read_u32(bytes, &mut pos)? as usize;
    let mut pairs = Vec::with_capacity(count);
    for _ in 0..count {
        let pattern = String::from_utf8(read_bytes(bytes, &mut pos)?)
            .map_err(|_| "pattern is not UTF-8".to_owned())?;
        let input = read_bytes(bytes, &mut pos)?;
        let span_count = read_u32(bytes, &mut pos)? as usize;
        let mut spans = Vec::with_capacity(span_count);
        for _ in 0..span_count {
            let start = read_u32(bytes, &mut pos)? as usize;
            let end = read_u32(bytes, &mut pos)? as usize;
            spans.push(Span::new(start, end));
        }
        pairs.push(GoldenPair {
            pattern,
            input,
            spans,
        });
    }
    Ok(pairs)
}

/// Total haystack bytes across a corpus (the `count` work unit).
pub fn corpus_bytes(pairs: &[Pair]) -> u64 {
    pairs.iter().map(|p| p.input.len() as u64).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_round_trips() {
        let pairs = vec![
            Pair {
                pattern: "a+".to_owned(),
                input: b"aaa".to_vec(),
            },
            Pair {
                pattern: r"\d+".to_owned(),
                input: b"x42y".to_vec(),
            },
        ];
        let bytes = serialize_corpus(&pairs);
        assert_eq!(deserialize_corpus(&bytes).unwrap(), pairs);
    }

    #[test]
    fn golden_round_trips() {
        let pairs = vec![GoldenPair {
            pattern: "a*".to_owned(),
            input: b"aa".to_vec(),
            spans: vec![Span::new(0, 2)],
        }];
        let bytes = serialize_golden(&pairs);
        assert_eq!(deserialize_golden(&bytes).unwrap(), pairs);
    }

    #[test]
    fn rejects_wrong_magic() {
        assert!(deserialize_corpus(b"optiwork-corpus-v2\n....").is_err());
        assert!(deserialize_golden(b"optiwork-corpus-v1\n....").is_err());
    }
}
