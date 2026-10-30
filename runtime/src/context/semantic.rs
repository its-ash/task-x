//! Embedding search over text chunks: the project's files (`semantic_search`) and indexed library
//! docs (`docs_search`). Chunks are embedded once and kept by content hash, so a later update only
//! embeds the chunks that changed. Saved as JSON under the runtime's data folder.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Lines per code chunk, and the overlap between neighbours so a function cut in two is still found.
const CHUNK_LINES: usize = 60;
const OVERLAP_LINES: usize = 10;
/// Characters per docs chunk.
const DOC_CHUNK_CHARS: usize = 1_600;
/// Files larger than this, and chunks beyond the total, are not embedded.
const MAX_FILE_BYTES: usize = 300 * 1024;
pub const MAX_CHUNKS: usize = 20_000;
/// Texts per embedding request.
pub const BATCH: usize = 32;
/// Characters of one chunk sent to the embedding model.
const MAX_EMBED_CHARS: usize = 6_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// A workspace path, or a docs page URL.
    pub source: String,
    /// Lines (code) or the page title (docs).
    pub start: usize,
    pub end: usize,
    #[serde(default)]
    pub title: String,
    pub hash: u64,
    #[serde(default)]
    pub text: String,
    pub vec: Vec<f32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    /// The embedding model that made the vectors; another model starts over.
    pub model: String,
    pub chunks: Vec<Chunk>,
}

pub fn hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// A piece of text to embed, before it has a vector.
pub struct Piece {
    pub source: String,
    pub start: usize,
    pub end: usize,
    pub title: String,
    pub text: String,
}

/// Code files in overlapping windows of lines; binary and very large files are skipped.
pub fn code_pieces(rel: &str, content: &str) -> Vec<Piece> {
    if content.len() > MAX_FILE_BYTES || content.contains('\0') {
        return Vec::new();
    }
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let end = (start + CHUNK_LINES).min(lines.len());
        let text = lines[start..end].join("\n");
        if !text.trim().is_empty() {
            out.push(Piece { source: rel.to_owned(), start: start + 1, end, title: String::new(), text });
        }
        if end == lines.len() {
            break;
        }
        start = end - OVERLAP_LINES;
    }
    out
}

/// Docs page text in chunks of about `DOC_CHUNK_CHARS`, split at paragraph ends where it can.
pub fn doc_pieces(url: &str, title: &str, text: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<Piece>| {
        if !cur.trim().is_empty() {
            let n = out.len();
            out.push(Piece { source: url.to_owned(), start: n + 1, end: n + 1, title: title.to_owned(), text: std::mem::take(cur).trim().to_owned() });
        }
        cur.clear();
    };
    for para in text.split("\n\n") {
        if cur.len() + para.len() > DOC_CHUNK_CHARS && !cur.is_empty() {
            flush(&mut cur, &mut out);
        }
        if para.len() > DOC_CHUNK_CHARS {
            for part in para.chars().collect::<Vec<_>>().chunks(DOC_CHUNK_CHARS) {
                cur.push_str(&part.iter().collect::<String>());
                flush(&mut cur, &mut out);
            }
        } else {
            cur.push_str(para);
            cur.push_str("\n\n");
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// What to send to the embedding model for a piece: its place and its text, cut to a safe size.
pub fn embed_text(p: &Piece) -> String {
    let head = if p.title.is_empty() { p.source.clone() } else { format!("{} ({})", p.title, p.source) };
    format!("{head}\n{}", p.text).chars().take(MAX_EMBED_CHARS).collect()
}

impl Store {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// The pieces that still need a vector, after keeping the vectors of unchanged ones. Chunks
    /// whose source is gone or changed are dropped. A different model starts over.
    pub fn plan(&mut self, model: &str, pieces: Vec<Piece>, keep_text: bool) -> Vec<Piece> {
        if self.model != model {
            self.model = model.to_owned();
            self.chunks.clear();
        }
        let mut known: HashMap<(String, u64), Chunk> = std::mem::take(&mut self.chunks).into_iter().map(|c| ((c.source.clone(), c.hash), c)).collect();
        let mut todo = Vec::new();
        for p in pieces.into_iter().take(MAX_CHUNKS) {
            let h = hash(&p.text);
            match known.remove(&(p.source.clone(), h)) {
                Some(mut c) => {
                    (c.start, c.end, c.title) = (p.start, p.end, p.title);
                    if keep_text {
                        c.text = p.text;
                    }
                    self.chunks.push(c);
                }
                None => todo.push(p),
            }
        }
        todo
    }

    pub fn add(&mut self, p: Piece, vec: Vec<f32>, keep_text: bool) {
        let h = hash(&p.text);
        self.chunks.push(Chunk { source: p.source, start: p.start, end: p.end, title: p.title, hash: h, text: if keep_text { p.text } else { String::new() }, vec });
    }

    /// The `k` chunks closest to the query vector, best first, with their cosine similarity.
    pub fn search(&self, query: &[f32], k: usize, filter: impl Fn(&Chunk) -> bool) -> Vec<(f32, &Chunk)> {
        let qn = norm(query);
        if qn == 0.0 {
            return Vec::new();
        }
        let mut hits: Vec<(f32, &Chunk)> = self
            .chunks
            .iter()
            .filter(|c| c.vec.len() == query.len() && filter(c))
            .map(|c| (dot(query, &c.vec) / (qn * norm(&c.vec)).max(f32::EPSILON), c))
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0));
        hits.truncate(k);
        hits
    }
}

/// Keyword search over stored text, for docs indexed without an embedding model: the share of the
/// query words a chunk contains, with whole-word hits counted twice.
pub fn keyword_search<'a>(chunks: &'a [Chunk], query: &str, k: usize, filter: impl Fn(&Chunk) -> bool) -> Vec<(f32, &'a Chunk)> {
    let words: Vec<String> = query.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| w.len() > 1).map(str::to_owned).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(f32, &Chunk)> = chunks
        .iter()
        .filter(|c| filter(c))
        .filter_map(|c| {
            let hay = format!("{} {}", c.title, c.text).to_lowercase();
            let score: f32 = words.iter().map(|w| if hay.split(|ch: char| !ch.is_alphanumeric() && ch != '_').any(|t| t == w) { 2.0 } else if hay.contains(w.as_str()) { 1.0 } else { 0.0 }).sum();
            (score > 0.0).then(|| (score / (2.0 * words.len() as f32), c))
        })
        .collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0));
    hits.truncate(k);
    hits
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

/// Where the code index of a workspace lives: one file per workspace root.
pub fn code_store_path(data_dir: &Path, root: &Path) -> PathBuf {
    data_dir.join("semantic").join(format!("{:016x}.json", hash(&root.to_string_lossy())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_cut_in_overlapping_windows() {
        let text: String = (1..=130).map(|i| format!("line {i}\n")).collect();
        let p = code_pieces("a.rs", &text);
        assert_eq!(p.iter().map(|p| (p.start, p.end)).collect::<Vec<_>>(), [(1, 60), (51, 110), (101, 130)]);
        assert!(code_pieces("b.bin", "a\0b").is_empty(), "binary files are skipped");
    }

    #[test]
    fn unchanged_chunks_keep_their_vectors_and_search_ranks_by_similarity() {
        let mut s = Store::default();
        let piece = |src: &str, text: &str| Piece { source: src.into(), start: 1, end: 1, title: String::new(), text: text.into() };
        let todo = s.plan("m", vec![piece("a", "colors"), piece("b", "config")], false);
        assert_eq!(todo.len(), 2);
        for (p, v) in todo.into_iter().zip([vec![1.0, 0.0], vec![0.0, 1.0]]) {
            s.add(p, v, false);
        }
        assert!(s.plan("m", vec![piece("a", "colors"), piece("b", "config v2")], false).len() == 1, "only the changed chunk is embedded again");
        assert_eq!(s.chunks.len(), 1);
        s.add(piece("b", "config v2"), vec![0.1, 0.9], false);
        let hits = s.search(&[0.0, 1.0], 1, |_| true);
        assert_eq!(hits[0].1.source, "b");
        assert!(s.plan("other-model", vec![piece("a", "colors")], false).len() == 1, "another model starts over");
    }

    #[test]
    fn keyword_search_prefers_whole_words() {
        let c = |t: &str| Chunk { source: t.into(), start: 1, end: 1, title: String::new(), hash: 0, text: t.into(), vec: Vec::new() };
        let chunks = [c("reqwest client builder"), c("the clientele"), c("nothing here")];
        let hits = keyword_search(&chunks, "client builder", 5, |_| true);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].1.source, "reqwest client builder");
    }
}
