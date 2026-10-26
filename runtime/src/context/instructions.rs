//! Loads the project instruction file.
//!
//! task-x sends one file: `CLAUDE.md` at the workspace root, else `.github/copilot-instructions.md`.
//! `CLAUDE.md` is usually a symlink to `.github/copilot-instructions.md`, so the two are the same
//! guidance and never go to the model twice. No other instruction files are read.
//! Instructions are guidance for the model; they never change permissions or security policy.

use super::index::Index;
use serde::Serialize;

/// Candidate files, in order; the first one that exists and is not empty is used.
pub const INSTRUCTION_FILES: &[&str] = &["CLAUDE.md", ".github/copilot-instructions.md"];
pub const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    Workspace,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instruction {
    pub source: String,
    pub scope: Scope,
    /// Root-relative directory the instruction applies to ("" = whole workspace).
    pub dir: String,
    pub content: String,
    pub truncated: bool,
}

fn clip(mut s: String) -> (String, bool) {
    if s.len() <= MAX_INSTRUCTION_BYTES {
        return (s, false);
    }
    let mut cut = MAX_INSTRUCTION_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    (s, true)
}

/// The instruction file of the workspace: at most one entry.
pub fn load(index: &Index) -> Vec<Instruction> {
    let root = index.root();
    INSTRUCTION_FILES
        .iter()
        .find_map(|rel| {
            let canon = root.join(rel).canonicalize().ok()?;
            // A symlink pointing outside the workspace is not followed.
            if !canon.starts_with(root) {
                return None;
            }
            let (content, truncated) = clip(std::fs::read_to_string(canon).ok()?);
            (!content.trim().is_empty()).then(|| Instruction {
                source: (*rel).into(),
                scope: Scope::Workspace,
                dir: String::new(),
                content,
                truncated,
            })
        })
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::index::tests::temp_repo;
    use super::*;

    fn loaded(idx: &mut Index) -> Vec<(String, String)> {
        idx.refresh();
        load(idx).into_iter().map(|i| (i.source, i.content)).collect()
    }

    #[test]
    fn sends_one_file_claude_md_first() {
        let root =
            temp_repo(&[(".github/copilot-instructions.md", "root rules"), ("AGENTS.md", "agents rules"), ("pkg/CLAUDE.md", "pkg rules")]);
        let mut idx = Index::new(root.clone(), &[]).unwrap();
        assert_eq!(
            loaded(&mut idx),
            [(".github/copilot-instructions.md".into(), "root rules".into())],
            "other instruction files are not read"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(".github/copilot-instructions.md", root.join("CLAUDE.md")).unwrap();
            assert_eq!(loaded(&mut idx), [("CLAUDE.md".into(), "root rules".into())], "the symlinked pair loads once");
            std::fs::remove_file(root.join("CLAUDE.md")).unwrap();
        }
        std::fs::write(root.join("CLAUDE.md"), "claude rules").unwrap();
        assert_eq!(loaded(&mut idx), [("CLAUDE.md".into(), "claude rules".into())], "CLAUDE.md wins over the Copilot file");
        std::fs::write(root.join("CLAUDE.md"), "  \n").unwrap();
        assert_eq!(loaded(&mut idx), [(".github/copilot-instructions.md".into(), "root rules".into())], "an empty CLAUDE.md is skipped");
        std::fs::remove_dir_all(root).ok();
    }
}
