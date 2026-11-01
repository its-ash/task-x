//! Workspace understanding: file index, project detection, instructions, and context retrieval.

pub mod graph;
pub mod index;
pub mod instructions;
pub mod project;
pub mod repomap;
pub mod retrieve;
pub mod semantic;
pub mod symbols;

use index::{Index, IndexStats};
use instructions::Instruction;
use project::ProjectInfo;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub struct Workspace {
    pub index: Index,
    pub project: ProjectInfo,
    pub instructions: Vec<Instruction>,
    /// Built on first use and dropped when files change.
    graph: OnceLock<Arc<graph::ProjectGraph>>,
    /// Where the graph is saved between runs (`<dataDir>/graphs/<root hash>.json`); unset: not saved.
    pub graph_file: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionSummary {
    pub source: String,
    pub scope: instructions::Scope,
    pub dir: String,
    pub bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary<'a> {
    pub project: &'a ProjectInfo,
    pub stats: &'a IndexStats,
    pub instructions: Vec<InstructionSummary>,
    pub excludes: &'a [String],
}

impl Workspace {
    pub fn open(root: &Path, excludes: &[String]) -> Result<Self, String> {
        Self::open_progress(root, excludes, &|_, _| {})
    }

    /// Like `open`, calling `progress(files seen, files parsed)` while it scans.
    pub fn open_progress(root: &Path, excludes: &[String], progress: &dyn Fn(usize, usize)) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|e| format!("cannot open workspace {}: {e}", root.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        let mut index = Index::new(root, excludes)?;
        index.refresh_with(progress);
        let mut ws = Self { project: ProjectInfo::default(), instructions: Vec::new(), index, graph: OnceLock::new(), graph_file: None };
        ws.reload_metadata();
        Ok(ws)
    }

    fn reload_metadata(&mut self) {
        self.project = project::detect(&self.index);
        self.instructions = instructions::load(&self.index);
    }

    pub fn refresh(&mut self) {
        self.index.refresh();
        self.graph = OnceLock::new();
        self.reload_metadata();
    }

    /// The project graph for the current index: the saved copy when it still matches, else built.
    pub fn graph(&self) -> Arc<graph::ProjectGraph> {
        self.graph.get_or_init(|| Arc::new(graph::ProjectGraph::load_or_build(&self.index, self.graph_file.as_deref()))).clone()
    }

    /// Finds what changed on disk since the last scan (edits, new and deleted files) and indexes it.
    /// Returns the changed root-relative paths.
    pub fn rescan(&mut self) -> Vec<String> {
        let paths = self.index.scan();
        if paths.is_empty() {
            return Vec::new();
        }
        self.files_changed(&paths);
        paths.iter().filter_map(|p| index::relative(self.index.root(), p)).collect()
    }

    pub fn files_changed(&mut self, paths: &[PathBuf]) -> usize {
        let n = self.index.update(paths);
        if n > 0 {
            self.graph = OnceLock::new();
        }
        let meta_changed = paths.iter().any(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            instructions::INSTRUCTION_FILES.iter().any(|f| f.ends_with(name))
                || matches!(name, "package.json" | "Cargo.toml" | "pyproject.toml" | "go.mod" | ".gitignore")
        });
        if meta_changed {
            self.reload_metadata();
        }
        n
    }

    pub fn summary(&self) -> Summary<'_> {
        Summary {
            project: &self.project,
            stats: self.index.stats(),
            instructions: self
                .instructions
                .iter()
                .map(|i| InstructionSummary {
                    source: i.source.clone(),
                    scope: i.scope,
                    dir: i.dir.clone(),
                    bytes: i.content.len(),
                    truncated: i.truncated,
                })
                .collect(),
            excludes: self.index.exclude_patterns(),
        }
    }
}
