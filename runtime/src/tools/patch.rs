//! Unified diff parsing and strict application (context must match; hunks may shift position).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// `None` means /dev/null (file creation or deletion).
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
    pub no_newline_at_end: bool,
}

fn strip_prefix(p: &str) -> Option<String> {
    let p = p.split('\t').next().unwrap_or(p).trim();
    if p == "/dev/null" {
        return None;
    }
    Some(p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_owned())
}

fn parse_range(s: &str) -> Result<usize, String> {
    let start = s.split(',').next().unwrap_or(s);
    start.parse::<usize>().map_err(|_| format!("bad hunk range '{s}'"))
}

pub fn parse(text: &str) -> Result<Vec<FilePatch>, String> {
    let mut out: Vec<FilePatch> = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(old) = line.strip_prefix("--- ") {
            let new = lines.next().and_then(|l| l.strip_prefix("+++ ")).ok_or("expected '+++' after '---'")?;
            out.push(FilePatch { old_path: strip_prefix(old), new_path: strip_prefix(new), hunks: Vec::new(), no_newline_at_end: false });
        } else if let Some(h) = line.strip_prefix("@@ ") {
            let fp = out.last_mut().ok_or("hunk before file header")?;
            let ranges = h.split(" @@").next().unwrap_or_default();
            let mut parts = ranges.split_whitespace();
            let old = parts.next().and_then(|p| p.strip_prefix('-')).ok_or("bad hunk header")?;
            let mut hunk = Hunk { old_start: parse_range(old)?, lines: Vec::new() };
            while let Some(l) = lines.peek() {
                if l.starts_with("@@ ") || l.starts_with("--- ") || l.starts_with("diff ") {
                    break;
                }
                let l = lines.next().unwrap_or_default();
                match l.chars().next() {
                    Some(' ') => hunk.lines.push(Line::Context(l[1..].to_owned())),
                    Some('-') => hunk.lines.push(Line::Remove(l[1..].to_owned())),
                    Some('+') => hunk.lines.push(Line::Add(l[1..].to_owned())),
                    Some('\\') => fp.no_newline_at_end = true,
                    None => hunk.lines.push(Line::Context(String::new())),
                    _ => return Err(format!("unexpected line in hunk: {l}")),
                }
            }
            fp.hunks.push(hunk);
        }
    }
    if out.is_empty() {
        return Err("no file headers ('--- a/path' / '+++ b/path') found".into());
    }
    Ok(out)
}

/// Applies hunks to `original`. Each hunk's old lines must match exactly, at the stated line or
/// uniquely elsewhere (searching outward from the stated position).
pub fn apply(original: &str, fp: &FilePatch) -> Result<String, String> {
    let crlf = original.contains("\r\n");
    let had_trailing_nl = original.is_empty() || original.ends_with('\n');
    let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
    let mut shift: isize = 0;
    for (n, h) in fp.hunks.iter().enumerate() {
        let old: Vec<&str> = h
            .lines
            .iter()
            .filter_map(|l| match l {
                Line::Context(s) | Line::Remove(s) => Some(s.as_str()),
                Line::Add(_) => None,
            })
            .collect();
        let new: Vec<String> = h
            .lines
            .iter()
            .filter_map(|l| match l {
                Line::Context(s) | Line::Add(s) => Some(s.clone()),
                Line::Remove(_) => None,
            })
            .collect();
        let expected = ((h.old_start.max(1) - 1) as isize + shift).max(0) as usize;
        let matches_at = |i: usize| i + old.len() <= lines.len() && lines[i..i + old.len()].iter().zip(&old).all(|(a, b)| a == b);
        let pos = if old.is_empty() {
            Some(expected.min(lines.len()))
        } else if matches_at(expected) {
            Some(expected)
        } else {
            let found: Vec<usize> = (0..=lines.len().saturating_sub(old.len())).filter(|&i| matches_at(i)).collect();
            match found.len() {
                1 => Some(found[0]),
                0 => None,
                _ => return Err(format!("hunk {} matches {} places; add more context lines", n + 1, found.len())),
            }
        };
        let Some(pos) = pos else {
            return Err(format!(
                "hunk {} (at line {}) does not match the file; read the file again and regenerate the patch",
                n + 1,
                h.old_start
            ));
        };
        shift += new.len() as isize - old.len() as isize;
        lines.splice(pos..pos + old.len(), new);
    }
    let nl = if crlf { "\r\n" } else { "\n" };
    let mut s = lines.join(nl);
    if !lines.is_empty() && had_trailing_nl && !fp.no_newline_at_end {
        s.push_str(nl);
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIG: &str = "a\nb\nc\nd\ne\n";

    #[test]
    fn parses_and_applies_shifted_hunks() {
        let p = parse("--- a/f.txt\n+++ b/f.txt\n@@ -2,2 +2,2 @@\n b\n-c\n+C\n@@ -5,1 +5,2 @@\n e\n+f\n").unwrap();
        assert_eq!(p[0].old_path.as_deref(), Some("f.txt"));
        assert_eq!(apply(ORIG, &p[0]).unwrap(), "a\nb\nC\nd\ne\nf\n");
        // Same patch still applies when lines were inserted above (position search).
        assert_eq!(apply("x\ny\na\nb\nc\nd\ne\n", &p[0]).unwrap(), "x\ny\na\nb\nC\nd\ne\nf\n");
    }

    #[test]
    fn rejects_mismatch_and_ambiguity_and_handles_creation() {
        let p = parse("--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-zzz\n+y\n").unwrap();
        assert!(apply(ORIG, &p[0]).unwrap_err().contains("does not match"));
        let p = parse("--- a/f\n+++ b/f\n@@ -9,1 +9,1 @@\n-x\n+y\n").unwrap();
        assert!(apply("x\nq\nx\n", &p[0]).unwrap_err().contains("2 places"));
        let p = parse("--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n").unwrap();
        assert_eq!(p[0].old_path, None);
        assert_eq!(apply("", &p[0]).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn preserves_crlf() {
        let p = parse("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n").unwrap();
        assert_eq!(apply("a\r\nz\r\n", &p[0]).unwrap(), "b\r\nz\r\n");
    }
}
