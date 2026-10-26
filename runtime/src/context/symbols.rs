//! Language detection and Tree-sitter symbol extraction using each grammar's `tags.scm` query.

use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
}

impl Lang {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript | Self::Tsx => "typescript",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
        }
    }

    fn grammar(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    fn tags_source(self) -> String {
        match self {
            // The TypeScript grammar extends JavaScript, so its tags build on the JavaScript ones.
            Self::TypeScript | Self::Tsx => format!("{}\n{}", tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY),
            Self::Rust => tree_sitter_rust::TAGS_QUERY.into(),
            Self::JavaScript => tree_sitter_javascript::TAGS_QUERY.into(),
            Self::Python => tree_sitter_python::TAGS_QUERY.into(),
            Self::Go => tree_sitter_go::TAGS_QUERY.into(),
        }
    }

    fn query(self) -> Option<&'static Query> {
        static CACHE: [OnceLock<Option<Query>>; 6] = [const { OnceLock::new() }; 6];
        CACHE[self as usize].get_or_init(|| Query::new(&self.grammar(), &self.tags_source()).ok()).as_ref()
    }
}

/// Human-facing language name for any file, including ones without a parser.
pub fn language_name(path: &Path) -> Option<&'static str> {
    if let Some(l) = parser_lang(path) {
        return Some(l.name());
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "scala" => "scala",
        "dart" => "dart",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "shell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" | "less" => "css",
        "vue" => "vue",
        "svelte" => "svelte",
        "md" | "mdx" => "markdown",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        _ => return None,
    })
}

pub fn parser_lang(path: &Path) -> Option<Lang> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => Lang::Rust,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "py" | "pyi" => Lang::Python,
        "go" => Lang::Go,
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Default, Clone)]
pub struct FileSymbols {
    pub definitions: Vec<Symbol>,
    /// Names referenced (calls, type uses) in this file.
    pub references: BTreeSet<String>,
    /// The subset referenced by a plain name, not through member access (`x.name`, `obj.name()`),
    /// which usually means a library type's method.
    pub direct: BTreeSet<String>,
}

/// Parent node kinds that make a name a member access in the supported grammars.
const MEMBER_PARENTS: &[&str] = &["field_expression", "member_expression", "attribute", "selector_expression"];

pub fn extract(lang: Lang, source: &str) -> FileSymbols {
    let Some(query) = lang.query() else { return FileSymbols::default() };
    let mut parser = Parser::new();
    if parser.set_language(&lang.grammar()).is_err() {
        return FileSymbols::default();
    }
    let Some(tree) = parser.parse(source, None) else { return FileSymbols::default() };
    let names = query.capture_names();
    let mut out = FileSymbols::default();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let mut name = None;
        let mut role = None;
        for c in m.captures() {
            let cap = names[c.index as usize];
            if cap == "name" {
                name = c.node.utf8_text(source.as_bytes()).ok();
            } else if let Some(kind) = cap.strip_prefix("definition.") {
                role = Some((true, kind, c.node));
            } else if let Some(kind) = cap.strip_prefix("reference.") {
                role = Some((false, kind, c.node));
            }
        }
        let member = m
            .captures()
            .iter()
            .any(|c| names[c.index as usize] == "name" && c.node.parent().is_some_and(|p| MEMBER_PARENTS.contains(&p.kind())));
        let (Some(name), Some((is_def, kind, node))) = (name, role) else { continue };
        if name.is_empty() || name.len() > 200 {
            continue;
        }
        if is_def {
            let sym = Symbol {
                name: name.to_owned(),
                kind: kind.to_owned(),
                start_line: node.start_position().row as u32 + 1,
                end_line: node.end_position().row as u32 + 1,
            };
            if out.definitions.last() != Some(&sym) {
                out.definitions.push(sym);
            }
        } else {
            out.references.insert(name.to_owned());
            if !member {
                out.direct.insert(name.to_owned());
            }
        }
    }
    out.definitions.sort_by(|a, b| (a.start_line, &a.name).cmp(&(b.start_line, &b.name)));
    out.definitions.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs(lang: Lang, src: &str) -> Vec<(String, String)> {
        extract(lang, src).definitions.into_iter().map(|s| (s.name, s.kind)).collect()
    }

    #[test]
    fn every_bundled_query_compiles() {
        for l in [Lang::Rust, Lang::TypeScript, Lang::Tsx, Lang::JavaScript, Lang::Python, Lang::Go] {
            assert!(l.query().is_some(), "{l:?} tags query failed to compile");
        }
    }

    #[test]
    fn extracts_rust_definitions_and_calls() {
        let src = "struct Index;\nimpl Index {\n    fn build() -> Self {\n        helper();\n        Index\n    }\n}\nfn helper() {}\n";
        let s = extract(Lang::Rust, src);
        let d: Vec<_> = s.definitions.iter().map(|d| (d.name.as_str(), d.start_line, d.end_line)).collect();
        assert!(d.contains(&("Index", 1, 1)));
        assert!(d.contains(&("build", 3, 6)));
        assert!(d.contains(&("helper", 8, 8)));
        assert!(s.references.contains("helper") && s.direct.contains("helper"));
        let m = extract(Lang::Rust, "fn f(c: Command) { c.args(); run(); }\n");
        assert!(m.references.contains("args") && !m.direct.contains("args"), "method calls are not direct references");
        assert!(m.direct.contains("run"));
        let t = extract(Lang::TypeScript, "panel.show(); render();\n");
        assert!(!t.direct.contains("show") && t.direct.contains("render"));
    }

    #[test]
    fn extracts_typescript_and_python() {
        let ts = defs(
            Lang::TypeScript,
            "export class Client { send(x: number) { return x } }\ninterface Opts { a: string }\nfunction go() {}\n",
        );
        for n in ["Client", "send", "Opts", "go"] {
            assert!(ts.iter().any(|(name, _)| name == n), "missing {n} in {ts:?}");
        }
        let py = defs(Lang::Python, "class A:\n    def m(self):\n        pass\n\ndef f():\n    return A()\n");
        assert!(py.contains(&("A".into(), "class".into())));
        assert!(py.contains(&("f".into(), "function".into())));
    }

    #[test]
    fn detects_languages_by_extension() {
        assert_eq!(parser_lang(Path::new("a/b.tsx")), Some(Lang::Tsx));
        assert_eq!(language_name(Path::new("x.kt")), Some("kotlin"));
        assert_eq!(language_name(Path::new("Makefile")), None);
    }
}
