//! Project graph: file-to-file dependencies built from the index. Import edges come from resolved
//! import statements (Rust, TypeScript/JavaScript, Python, Go); reference edges link a file to the
//! file that defines a symbol it uses (every tree-sitter language). A reference links only when
//! exactly one file of the same language family defines the name as an exported, non-method
//! symbol: other files cannot use private items, method calls usually target library types, and
//! shared names (`new`, `run`, `Host`) would link noise.
//! On top of the file graph:
//! a directory (module) roll-up, hub files, orphans, and import cycles.

use super::index::Index;
use super::symbols::{Lang, Symbol, parser_lang};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

const GRAPH_ENGINE_VERSION: u32 = 2;

const MIN_NAME_LEN: usize = 3;
const MAX_VIEW_NODES: usize = 600;
const DEFAULT_VIEW_NODES: usize = 250;
const MAX_DEPTH: usize = 4;
const HUBS: usize = 10;
const MAX_CYCLES: usize = 20;
const TS_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EdgeKind {
    Import,
    Reference,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub path: String,
    pub dir: String,
    pub language: Option<&'static str>,
    pub symbols: usize,
    pub size: u64,
}

/// `weight` counts distinct linked symbols (at least 1 for an import).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub kind: EdgeKind,
    pub weight: u32,
}

pub struct ProjectGraph {
    pub version: u32,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    ids: HashMap<String, usize>,
    out: Vec<Vec<usize>>,
    inc: Vec<Vec<usize>>,
    /// Import cycles (strongly connected components with more than one file), largest first.
    pub cycles: Vec<Vec<usize>>,
    pub build_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    #[default]
    File,
    Module,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ViewOptions {
    #[serde(default)]
    pub level: Level,
    /// Show only nodes within `depth` steps of this file or directory.
    #[serde(default)]
    pub focus: Option<String>,
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub max_nodes: Option<usize>,
    /// Leave out reference edges (imports only).
    #[serde(default)]
    pub imports_only: bool,
}

/// Languages whose files can use each other's symbols.
fn family(rel: &str) -> Option<u8> {
    Some(match parser_lang(Path::new(rel))? {
        Lang::Rust => 0,
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => 1,
        Lang::Python => 2,
        Lang::Go => 3,
    })
}

/// Whether a definition is visible to other files: `pub` in Rust, `export` in TypeScript and
/// JavaScript, a capitalized name in Go, no leading underscore in Python.
fn exported(index: &Index, path: &str, s: &Symbol) -> bool {
    let line = index
        .get(path)
        .and_then(|f| f.content.as_deref())
        .and_then(|c| c.lines().nth(s.start_line.saturating_sub(1) as usize))
        .unwrap_or("")
        .trim_start();
    match parser_lang(Path::new(path)) {
        Some(Lang::Rust) => line.starts_with("pub"),
        Some(Lang::TypeScript | Lang::Tsx | Lang::JavaScript) => line.starts_with("export"),
        Some(Lang::Go) => s.name.starts_with(|c: char| c.is_ascii_uppercase()),
        Some(Lang::Python) => !s.name.starts_with('_'),
        None => false,
    }
}

fn dir_of(rel: &str) -> &str {
    rel.rfind('/').map_or(".", |i| &rel[..i])
}

/// Joins a `/`-separated relative path onto a directory and folds `.` and `..`; None when it leaves the root.
fn join(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir == "." { Vec::new() } else { dir.split('/').collect() };
    for p in spec.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid import regex"))
}

/// Import targets of one file that exist in the index.
fn imports(rel: &str, content: &str, has: &impl Fn(&str) -> bool, go_dirs: &HashMap<String, String>) -> Vec<String> {
    let Some(lang) = parser_lang(Path::new(rel)) else { return Vec::new() };
    let dir = dir_of(rel);
    let first = |cands: Vec<String>| cands.into_iter().find(|c| has(c));
    let mut out = Vec::new();
    match lang {
        Lang::Rust => {
            static MOD: OnceLock<Regex> = OnceLock::new();
            static USE: OnceLock<Regex> = OnceLock::new();
            // A module's children live next to it (lib.rs, main.rs, mod.rs) or in a folder named after it.
            let file = rel.rsplit('/').next().unwrap_or(rel);
            let mod_dir = match file {
                "lib.rs" | "main.rs" | "mod.rs" => dir.to_owned(),
                f => join(dir, f.trim_end_matches(".rs")).unwrap_or_default(),
            };
            for c in re(&MOD, r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;").captures_iter(content) {
                out.extend(first(vec![format!("{mod_dir}/{}.rs", &c[1]), format!("{mod_dir}/{}/mod.rs", &c[1])]));
            }
            let src = rel.rfind("src/").map(|i| &rel[..i + 3]);
            for c in re(&USE, r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+(crate|super|self)::([\w:]+)").captures_iter(content) {
                let base = match &c[1] {
                    "crate" => src.map(str::to_owned),
                    "super" => join(&mod_dir, ".."),
                    _ => Some(mod_dir.clone()),
                };
                let Some(base) = base else { continue };
                let segs: Vec<&str> = c[2].split("::").filter(|s| !s.is_empty()).collect();
                // The longest prefix of the path that names a module file.
                for n in (1..=segs.len()).rev() {
                    let p = segs[..n].join("/");
                    if let Some(hit) = first(vec![format!("{base}/{p}.rs"), format!("{base}/{p}/mod.rs")]) {
                        out.push(hit);
                        break;
                    }
                }
            }
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            static SPEC: OnceLock<Regex> = OnceLock::new();
            let r = re(&SPEC, r#"(?:\bfrom\s*|\bimport\s*\(?\s*|\brequire\s*\(\s*)['"](\.{1,2}/[^'"]+)['"]"#);
            for c in r.captures_iter(content) {
                let Some(base) = join(dir, &c[1]) else { continue };
                // ESM TypeScript writes "./x.js" for "./x.ts".
                let stem = base.strip_suffix(".js").or_else(|| base.strip_suffix(".mjs")).unwrap_or(&base).to_owned();
                let mut cands = vec![base.clone()];
                cands.extend(TS_EXTS.iter().map(|e| format!("{stem}.{e}")));
                cands.extend(TS_EXTS.iter().map(|e| format!("{base}/index.{e}")));
                out.extend(first(cands));
            }
        }
        Lang::Python => {
            static FROM: OnceLock<Regex> = OnceLock::new();
            static IMPORT: OnceLock<Regex> = OnceLock::new();
            let module = |base: &str, m: &str| {
                let p = m.replace('.', "/");
                let p = if p.is_empty() {
                    base.to_owned()
                } else if base.is_empty() || base == "." {
                    p
                } else {
                    format!("{base}/{p}")
                };
                first(vec![format!("{p}.py"), format!("{p}/__init__.py"), format!("src/{p}.py"), format!("src/{p}/__init__.py")])
            };
            for c in re(&FROM, r"(?m)^\s*from\s+(\.*)([\w.]*)\s+import\s+([\w, ]+)").captures_iter(content) {
                let dots = c[1].len();
                let base =
                    if dots == 0 { String::new() } else { (1..dots).try_fold(dir.to_owned(), |d, _| join(&d, "..")).unwrap_or_default() };
                match module(&base, &c[2]) {
                    // `from . import a, b` imports sibling modules.
                    Some(hit) if !c[2].is_empty() => out.push(hit),
                    _ => {
                        out.extend(c[3].split(',').filter_map(|n| module(&base, format!("{}.{}", &c[2], n.trim()).trim_start_matches('.'))))
                    }
                }
            }
            for c in re(&IMPORT, r"(?m)^\s*import\s+([\w.]+)").captures_iter(content) {
                out.extend(module("", &c[1]));
            }
        }
        Lang::Go => {
            static GO: OnceLock<Regex> = OnceLock::new();
            for c in re(&GO, r#""([\w.\-/]+)""#).captures_iter(content.split(")\n").next().unwrap_or(content)) {
                let spec = &c[1];
                if let Some((_, f)) =
                    go_dirs.iter().filter(|(d, _)| spec == d.as_str() || spec.ends_with(&format!("/{d}"))).max_by_key(|(d, _)| d.len())
                {
                    out.push(f.clone());
                }
            }
        }
    }
    out.retain(|t| t != rel);
    out.sort();
    out.dedup();
    out
}

impl ProjectGraph {
    pub fn build(index: &Index) -> Self {
        let started = Instant::now();
        let files: Vec<_> = index.files().collect();
        let ids: HashMap<String, usize> = files.iter().enumerate().map(|(i, f)| (f.rel.clone(), i)).collect();
        let nodes = files
            .iter()
            .map(|f| Node {
                path: f.rel.clone(),
                dir: dir_of(&f.rel).to_owned(),
                language: f.language,
                symbols: f.symbols.definitions.len(),
                size: f.size,
            })
            .collect();
        // Go: one representative (non-test) file per package directory.
        let mut go_dirs: HashMap<String, String> = HashMap::new();
        for f in files.iter().filter(|f| f.rel.ends_with(".go") && !f.rel.ends_with("_test.go")) {
            go_dirs.entry(dir_of(&f.rel).to_owned()).or_insert_with(|| f.rel.clone());
        }
        let has = |p: &str| ids.contains_key(p);
        let mut merged: BTreeMap<(usize, usize), (EdgeKind, u32)> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            if let Some(c) = &f.content {
                for t in imports(&f.rel, c, &has, &go_dirs) {
                    merged.entry((i, ids[&t])).or_insert((EdgeKind::Import, 0)).1 += 1;
                }
            }
            let fam = family(&f.rel);
            for name in f.symbols.direct.iter().filter(|n| n.len() >= MIN_NAME_LEN) {
                let defs = index.definitions(name);
                if fam.is_none() || defs.iter().any(|(p, _)| *p == f.rel) {
                    continue;
                }
                let files: HashSet<&str> = defs
                    .iter()
                    .filter(|(p, s)| s.kind != "method" && family(p) == fam && exported(index, p, s))
                    .map(|(p, _)| p.as_str())
                    .collect();
                if let [t] = files.into_iter().collect::<Vec<_>>()[..] {
                    merged.entry((i, ids[t])).or_insert((EdgeKind::Reference, 0)).1 += 1;
                }
            }
        }
        let edges: Vec<Edge> = merged.into_iter().map(|((from, to), (kind, w))| Edge { from, to, kind, weight: w.max(1) }).collect();
        let mut out = vec![Vec::new(); files.len()];
        let mut inc = vec![Vec::new(); files.len()];
        for (k, e) in edges.iter().enumerate() {
            out[e.from].push(k);
            inc[e.to].push(k);
        }
        let mut g = Self { version: GRAPH_ENGINE_VERSION, nodes, edges, ids, out, inc, cycles: Vec::new(), build_ms: 0 };
        g.cycles = g.import_cycles();
        g.build_ms = started.elapsed().as_millis() as u64;
        g
    }

    pub fn id(&self, path: &str) -> Option<usize> {
        self.ids.get(path.trim_start_matches("./")).copied()
    }

    /// Strongly connected components of the import edges (iterative Tarjan).
    fn import_cycles(&self) -> Vec<Vec<usize>> {
        let n = self.nodes.len();
        let succ = |v: usize| self.out[v].iter().map(|&k| &self.edges[k]).filter(|e| e.kind == EdgeKind::Import).map(|e| e.to);
        let (mut index, mut low, mut on) = (vec![usize::MAX; n], vec![0; n], vec![false; n]);
        let (mut stack, mut next, mut sccs) = (Vec::new(), 0, Vec::new());
        for root in 0..n {
            if index[root] != usize::MAX {
                continue;
            }
            let mut work: Vec<(usize, Vec<usize>)> = vec![(root, succ(root).collect())];
            index[root] = next;
            low[root] = next;
            next += 1;
            stack.push(root);
            on[root] = true;
            while let Some((v, rest)) = work.last_mut() {
                let v = *v;
                if let Some(w) = rest.pop() {
                    if index[w] == usize::MAX {
                        index[w] = next;
                        low[w] = next;
                        next += 1;
                        stack.push(w);
                        on[w] = true;
                        work.push((w, succ(w).collect()));
                    } else if on[w] {
                        low[v] = low[v].min(index[w]);
                    }
                    continue;
                }
                work.pop();
                if let Some((u, _)) = work.last() {
                    low[*u] = low[*u].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    if comp.len() > 1 {
                        comp.sort_by(|a, b| self.nodes[*a].path.cmp(&self.nodes[*b].path));
                        sccs.push(comp);
                    }
                }
            }
        }
        sccs.sort_by_key(|c| std::cmp::Reverse(c.len()));
        sccs
    }

    fn in_degree(&self, v: usize) -> usize {
        self.inc[v].len()
    }

    /// Counts and highlights for status displays and the agent.
    pub fn stats(&self) -> Value {
        let imports = self.edges.iter().filter(|e| e.kind == EdgeKind::Import).count();
        let modules: HashSet<&str> = self.nodes.iter().map(|n| n.dir.as_str()).collect();
        let mut hubs: Vec<usize> = (0..self.nodes.len()).filter(|&v| self.in_degree(v) > 0).collect();
        hubs.sort_by_key(|&v| (std::cmp::Reverse(self.in_degree(v)), self.nodes[v].path.clone()));
        let orphans =
            (0..self.nodes.len()).filter(|&v| self.out[v].is_empty() && self.inc[v].is_empty() && self.nodes[v].symbols > 0).count();
        json!({
            "files": self.nodes.len(),
            "edges": self.edges.len(),
            "importEdges": imports,
            "referenceEdges": self.edges.len() - imports,
            "modules": modules.len(),
            "cycles": self.cycles.len(),
            "orphans": orphans,
            "hubs": hubs.iter().take(HUBS).map(|&v| json!({ "path": self.nodes[v].path, "dependents": self.in_degree(v) })).collect::<Vec<_>>(),
            "importCycles": self.cycles.iter().take(MAX_CYCLES).map(|c| c.iter().map(|&i| self.nodes[i].path.clone()).collect::<Vec<_>>()).collect::<Vec<_>>(),
            "buildMs": self.build_ms,
            "version": self.version,
        })
    }

    /// What one file depends on and what depends on it.
    pub fn neighbors(&self, path: &str) -> Option<Value> {
        let v = self.id(path)?;
        let side = |list: &[usize], pick: fn(&Edge) -> usize| {
            let mut rows: Vec<&Edge> = list.iter().map(|&k| &self.edges[k]).collect();
            rows.sort_by_key(|e| (e.kind != EdgeKind::Import, std::cmp::Reverse(e.weight)));
            rows.iter().map(|e| json!({ "path": self.nodes[pick(e)].path, "kind": e.kind, "symbols": e.weight })).collect::<Vec<_>>()
        };
        let cycle = self.cycles.iter().find(|c| c.contains(&v)).map(|c| c.iter().map(|&i| self.nodes[i].path.clone()).collect::<Vec<_>>());
        Some(json!({
            "path": self.nodes[v].path,
            "dependsOn": side(&self.out[v], |e| e.to),
            "usedBy": side(&self.inc[v], |e| e.from),
            "importCycle": cycle,
        }))
    }

    /// A drawable graph: nodes and edges at file or module level, trimmed to the most connected
    /// nodes (or to the neighborhood of `focus`).
    pub fn view(&self, o: &ViewOptions) -> Value {
        let max = o.max_nodes.unwrap_or(DEFAULT_VIEW_NODES).clamp(1, MAX_VIEW_NODES);
        let keep_edge = |e: &Edge| !o.imports_only || e.kind == EdgeKind::Import;
        let cyclic: HashSet<usize> = self.cycles.iter().flatten().copied().collect();
        // Map every file to a view node (itself, or its directory).
        let (keys, of): (Vec<String>, Vec<usize>) = match o.level {
            Level::File => (self.nodes.iter().map(|n| n.path.clone()).collect(), (0..self.nodes.len()).collect()),
            Level::Module => {
                let mut keys: Vec<String> = Vec::new();
                let mut pos: HashMap<&str, usize> = HashMap::new();
                let of = self
                    .nodes
                    .iter()
                    .map(|n| {
                        *pos.entry(n.dir.as_str()).or_insert_with(|| {
                            keys.push(n.dir.clone());
                            keys.len() - 1
                        })
                    })
                    .collect();
                (keys, of)
            }
        };
        let mut weights: BTreeMap<(usize, usize), (u32, bool)> = BTreeMap::new();
        for e in self.edges.iter().filter(|e| keep_edge(e)) {
            let (a, b) = (of[e.from], of[e.to]);
            if a != b {
                let w = weights.entry((a, b)).or_insert((0, false));
                w.0 += e.weight;
                w.1 |= e.kind == EdgeKind::Import;
            }
        }
        let n = keys.len();
        let mut degree = vec![0usize; n];
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &(a, b) in weights.keys() {
            degree[a] += 1;
            degree[b] += 1;
            adj[a].push(b);
            adj[b].push(a);
        }
        let mut chosen: Vec<usize> = match o.focus.as_deref().map(|f| f.trim_start_matches("./").trim_end_matches('/')) {
            Some(f) => {
                let start: Vec<usize> = (0..n).filter(|&i| keys[i] == f || keys[i].starts_with(&format!("{f}/"))).collect();
                let depth = o.depth.unwrap_or(1).min(MAX_DEPTH);
                let mut dist = vec![usize::MAX; n];
                let mut q: VecDeque<usize> = start.iter().copied().collect();
                start.iter().for_each(|&s| dist[s] = 0);
                while let Some(v) = q.pop_front() {
                    if dist[v] < depth {
                        for &w in &adj[v] {
                            if dist[w] == usize::MAX {
                                dist[w] = dist[v] + 1;
                                q.push_back(w);
                            }
                        }
                    }
                }
                let mut c: Vec<usize> = (0..n).filter(|&i| dist[i] != usize::MAX).collect();
                c.sort_by_key(|&i| (dist[i], std::cmp::Reverse(degree[i])));
                c
            }
            None => {
                // Unlinked files would only form a ring around the graph; they show when focused.
                let linked = o.level == Level::Module || degree.iter().all(|&d| d == 0);
                let mut c: Vec<usize> = (0..n).filter(|&i| linked || degree[i] > 0).collect();
                c.sort_by_key(|&i| (std::cmp::Reverse(degree[i]), keys[i].clone()));
                c
            }
        };
        let total = chosen.len();
        chosen.truncate(max);
        let pos: HashMap<usize, usize> = chosen.iter().enumerate().map(|(p, &i)| (i, p)).collect();
        let mut members: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (f, &k) in of.iter().enumerate() {
            members[k].push(f);
        }
        let files_in = |k: usize| members[k].iter().copied();
        let nodes: Vec<Value> = chosen
            .iter()
            .map(|&k| {
                let (lang, symbols, files) = match o.level {
                    Level::File => (self.nodes[k].language, self.nodes[k].symbols, 1),
                    Level::Module => {
                        let mut langs: HashMap<&str, usize> = HashMap::new();
                        let (mut s, mut c) = (0, 0);
                        for f in files_in(k) {
                            s += self.nodes[f].symbols;
                            c += 1;
                            if let Some(l) = self.nodes[f].language {
                                *langs.entry(l).or_default() += 1;
                            }
                        }
                        (langs.into_iter().max_by_key(|(_, c)| *c).map(|(l, _)| l), s, c)
                    }
                };
                let in_cycle = match o.level {
                    Level::File => cyclic.contains(&k),
                    Level::Module => files_in(k).any(|f| cyclic.contains(&f)),
                };
                json!({ "id": keys[k], "language": lang, "symbols": symbols, "files": files, "degree": degree[k], "inCycle": in_cycle })
            })
            .collect();
        let edges: Vec<Value> = weights
            .iter()
            .filter_map(|(&(a, b), &(w, imp))| {
                Some(json!({ "from": pos.get(&a)?, "to": pos.get(&b)?, "weight": w, "kind": if imp { "import" } else { "reference" } }))
            })
            .collect();
        json!({ "nodes": nodes, "edges": edges, "total": total, "stats": self.stats() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::index::tests::temp_repo;

    fn graph(files: &[(&str, &str)]) -> (ProjectGraph, std::path::PathBuf) {
        let root = temp_repo(files);
        let mut ix = Index::new(root.clone(), &[]).unwrap();
        ix.refresh();
        (ProjectGraph::build(&ix), root)
    }

    fn deps(g: &ProjectGraph, p: &str) -> Vec<String> {
        g.neighbors(p).unwrap()["dependsOn"].as_array().unwrap().iter().map(|d| d["path"].as_str().unwrap().to_owned()).collect()
    }

    #[test]
    fn resolves_imports_across_languages_and_finds_cycles() {
        let (g, root) = graph(&[
            ("rs/src/main.rs", "mod util;\nuse crate::net::client::Client;\nfn main() {}\n"),
            ("rs/src/util.rs", "pub fn helper() {}\n"),
            ("rs/src/net/mod.rs", "pub mod client;\n"),
            ("rs/src/net/client.rs", "pub struct Client;\n"),
            ("web/app.ts", "import { b } from './lib/b.js';\nimport c from \"./c\";\nexport const a = 1;\n"),
            ("web/lib/b.ts", "import { a } from '../app';\nexport const b = 2;\n"),
            ("web/c/index.tsx", "export default 3;\n"),
            ("py/pkg/__init__.py", ""),
            ("py/pkg/core.py", "from . import util\nfrom .models import User\n"),
            ("py/pkg/util.py", "def f():\n    pass\n"),
            ("py/pkg/models.py", "class User:\n    pass\n"),
        ]);
        assert_eq!(deps(&g, "rs/src/main.rs"), ["rs/src/net/client.rs", "rs/src/util.rs"]);
        assert_eq!(deps(&g, "rs/src/net/mod.rs"), ["rs/src/net/client.rs"]);
        assert_eq!(deps(&g, "web/app.ts"), ["web/c/index.tsx", "web/lib/b.ts"]);
        assert_eq!(deps(&g, "py/pkg/core.py"), ["py/pkg/models.py", "py/pkg/util.py"]);
        assert_eq!(g.cycles.len(), 1, "app.ts and lib/b.ts import each other");
        assert!(g.neighbors("web/app.ts").unwrap()["importCycle"].is_array());
        let s = g.stats();
        assert!(s["importEdges"].as_u64().unwrap() >= 8);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn reference_edges_module_view_and_focus() {
        let (g, root) = graph(&[
            ("src/a.rs", "pub fn alpha() { beta(); gamma(); }\n"),
            ("src/b.rs", "pub fn beta() {}\n"),
            ("lib/c.rs", "pub fn gamma() {}\n"),
            ("lib/d.rs", "pub fn delta() {}\n"),
        ]);
        let d = g.neighbors("src/a.rs").unwrap();
        assert_eq!(d["dependsOn"].as_array().unwrap().len(), 2);
        assert_eq!(d["dependsOn"][0]["kind"], "reference");
        std::fs::remove_dir_all(&root).ok();
        // No links through methods, names defined in two files, or other languages.
        let (g, root) = graph(&[
            ("a.ts", "class Panel { show() {} }\nexport function render() {}\n"),
            ("b.ts", "panel.show(); render();\n"),
            ("x.rs", "pub struct Host;\nfn helper() {}\n"),
            ("y.rs", "pub struct Host;\n"),
            ("z.rs", "fn f(h: Host) { render(); helper(); }\n"),
        ]);
        let b = g.neighbors("b.ts").unwrap();
        assert_eq!(b["dependsOn"].as_array().unwrap().len(), 1);
        assert_eq!(b["dependsOn"][0]["symbols"], 1, "only render(), not the show() method");
        assert!(
            g.neighbors("z.rs").unwrap()["dependsOn"].as_array().unwrap().is_empty(),
            "Host is ambiguous, helper() is private, render() is TypeScript"
        );
        std::fs::remove_dir_all(&root).ok();
        let (g, root) = graph(&[
            ("src/a.rs", "pub fn alpha() { beta(); gamma(); }\n"),
            ("src/b.rs", "pub fn beta() {}\n"),
            ("lib/c.rs", "pub fn gamma() {}\n"),
            ("lib/d.rs", "pub fn delta() {}\n"),
        ]);
        let m = g.view(&ViewOptions { level: Level::Module, ..Default::default() });
        assert_eq!(m["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(m["edges"].as_array().unwrap().len(), 1, "src -> lib");
        let f = g.view(&ViewOptions { focus: Some("lib/c.rs".into()), depth: Some(1), ..Default::default() });
        let ids: Vec<&str> = f["nodes"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["lib/c.rs", "src/a.rs"]);
        std::fs::remove_dir_all(root).ok();
    }
}
