//! Detects languages, manifests, package managers, frameworks, and project commands.

use super::index::Index;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LanguageStat {
    pub language: String,
    pub files: usize,
}

/// A command as an argv array plus the directory (relative to root) it runs in. Never a shell string.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub argv: Vec<String>,
    pub cwd: String,
    pub source: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCommands {
    pub build: Vec<Command>,
    pub test: Vec<Command>,
    pub lint: Vec<Command>,
    pub format: Vec<Command>,
    pub typecheck: Vec<Command>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub git_root: Option<PathBuf>,
    pub languages: Vec<LanguageStat>,
    pub manifests: Vec<String>,
    pub package_managers: Vec<String>,
    pub frameworks: Vec<String>,
    pub build_systems: Vec<String>,
    pub test_frameworks: Vec<String>,
    pub commands: ProjectCommands,
}

pub fn git_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "composer.json",
    "pubspec.yaml",
    "Package.swift",
    "CMakeLists.txt",
    "Makefile",
    "deno.json",
];

const JS_FRAMEWORKS: &[(&str, &str)] = &[
    ("next", "next.js"),
    ("nuxt", "nuxt"),
    ("react", "react"),
    ("vue", "vue"),
    ("svelte", "svelte"),
    ("@angular/core", "angular"),
    ("astro", "astro"),
    ("express", "express"),
    ("fastify", "fastify"),
    ("@nestjs/core", "nestjs"),
    ("electron", "electron"),
    ("@types/vscode", "vscode-extension"),
];
const JS_TESTS: &[&str] = &["jest", "vitest", "mocha", "ava", "@playwright/test", "cypress"];
const RUST_FRAMEWORKS: &[&str] = &["tokio", "axum", "actix-web", "rocket", "tauri", "bevy", "warp"];
const PY_FRAMEWORKS: &[&str] = &["django", "fastapi", "flask", "pytest", "starlette"];

fn cmd(argv: &[&str], cwd: &str, source: &str) -> Command {
    Command { argv: argv.iter().map(|s| s.to_string()).collect(), cwd: cwd.into(), source: source.into() }
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

pub fn detect(index: &Index) -> ProjectInfo {
    let root = index.root();
    let mut langs: BTreeMap<&str, usize> = BTreeMap::new();
    let mut manifests = Vec::new();
    for f in index.files() {
        if let Some(l) = f.language.filter(|l| !matches!(*l, "markdown" | "json" | "yaml" | "toml")) {
            *langs.entry(l).or_default() += 1;
        }
        let name = f.rel.rsplit('/').next().unwrap_or(&f.rel);
        // Only manifests near the root count as project manifests (depth ≤ 2), to skip fixtures.
        if MANIFESTS.contains(&name) && f.rel.matches('/').count() <= 2 {
            manifests.push(f.rel.clone());
        }
    }
    let mut languages: Vec<LanguageStat> = langs.into_iter().map(|(l, n)| LanguageStat { language: l.into(), files: n }).collect();
    languages.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.language.cmp(&b.language)));

    let mut pms = BTreeSet::new();
    let mut fw = BTreeSet::new();
    let mut builds = BTreeSet::new();
    let mut tests = BTreeSet::new();
    let mut cmds = ProjectCommands::default();
    let exists = |dir: &str, f: &str| root.join(dir).join(f).exists();
    let content = |rel: &str| index.get(rel).and_then(|f| f.content.clone());

    for m in &manifests {
        let dir = dir_of(m);
        let src = m.as_str();
        match m.rsplit('/').next().unwrap_or(m) {
            "Cargo.toml" => {
                pms.insert("cargo");
                builds.insert("cargo");
                tests.insert("cargo test");
                let text = content(m).unwrap_or_default();
                for f in RUST_FRAMEWORKS {
                    if text.lines().any(|l| l.trim_start().starts_with(&format!("{f} ")) || l.trim_start().starts_with(&format!("{f}="))) {
                        fw.insert(*f);
                    }
                }
                // Workspace members share the root commands; skip nested crates under a workspace root.
                if dir.is_empty() || !exists("", "Cargo.toml") {
                    cmds.build.push(cmd(&["cargo", "build"], dir, src));
                    cmds.test.push(cmd(&["cargo", "test"], dir, src));
                    cmds.lint.push(cmd(&["cargo", "clippy", "--all-targets"], dir, src));
                    cmds.format.push(cmd(&["cargo", "fmt"], dir, src));
                    cmds.typecheck.push(cmd(&["cargo", "check"], dir, src));
                }
            }
            "package.json" => {
                let pm = if exists(dir, "pnpm-lock.yaml") {
                    "pnpm"
                } else if exists(dir, "yarn.lock") {
                    "yarn"
                } else if exists(dir, "bun.lockb") || exists(dir, "bun.lock") {
                    "bun"
                } else {
                    "npm"
                };
                pms.insert(pm);
                let pkg: Value = content(m).and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default();
                let has_dep = |d: &str| ["dependencies", "devDependencies", "peerDependencies"].iter().any(|k| pkg[k].get(d).is_some());
                for (dep, name) in JS_FRAMEWORKS {
                    if has_dep(dep) {
                        fw.insert(*name);
                    }
                }
                for t in JS_TESTS {
                    if has_dep(t) {
                        tests.insert(*t);
                    }
                }
                if has_dep("typescript") {
                    builds.insert("tsc");
                }
                for b in ["vite", "webpack", "esbuild", "rollup", "turbo"] {
                    if has_dep(b) {
                        builds.insert(b);
                    }
                }
                let scripts = pkg["scripts"].as_object();
                let script = |name: &str| scripts.is_some_and(|s| s.contains_key(name));
                let run = |name: &str| match pm {
                    "npm" => cmd(&["npm", "run", name], dir, src),
                    other => cmd(&[other, "run", name], dir, src),
                };
                if script("build") {
                    cmds.build.push(run("build"));
                }
                if script("test") {
                    cmds.test.push(run("test"));
                    if pkg["scripts"]["test"].as_str().is_some_and(|t| t.contains("node --test")) {
                        tests.insert("node:test");
                    }
                }
                if script("lint") {
                    cmds.lint.push(run("lint"));
                }
                if script("format") {
                    cmds.format.push(run("format"));
                }
                if script("typecheck") {
                    cmds.typecheck.push(run("typecheck"));
                }
            }
            "pyproject.toml" | "requirements.txt" | "setup.py" => {
                pms.insert(if exists(dir, "uv.lock") {
                    "uv"
                } else if exists(dir, "poetry.lock") {
                    "poetry"
                } else {
                    "pip"
                });
                let text = content(m).unwrap_or_default().to_ascii_lowercase();
                for f in PY_FRAMEWORKS {
                    if text.contains(f) {
                        if *f == "pytest" {
                            tests.insert("pytest");
                        } else {
                            fw.insert(*f);
                        }
                    }
                }
                if tests.contains("pytest") && cmds.test.iter().all(|c| c.argv[0] != "pytest") {
                    cmds.test.push(cmd(&["pytest"], dir, src));
                }
                if text.contains("ruff") {
                    cmds.lint.push(cmd(&["ruff", "check", "."], dir, src));
                    cmds.format.push(cmd(&["ruff", "format", "."], dir, src));
                }
            }
            "go.mod" => {
                pms.insert("go");
                builds.insert("go");
                tests.insert("go test");
                cmds.build.push(cmd(&["go", "build", "./..."], dir, src));
                cmds.test.push(cmd(&["go", "test", "./..."], dir, src));
                cmds.lint.push(cmd(&["go", "vet", "./..."], dir, src));
                cmds.format.push(cmd(&["gofmt", "-l", "-w", "."], dir, src));
            }
            "pom.xml" => {
                builds.insert("maven");
                cmds.build.push(cmd(&["mvn", "-q", "compile"], dir, src));
                cmds.test.push(cmd(&["mvn", "-q", "test"], dir, src));
            }
            "build.gradle" | "build.gradle.kts" => {
                builds.insert("gradle");
                let g = if exists(dir, "gradlew") { "./gradlew" } else { "gradle" };
                cmds.build.push(cmd(&[g, "build", "-x", "test"], dir, src));
                cmds.test.push(cmd(&[g, "test"], dir, src));
            }
            "Makefile" => {
                builds.insert("make");
            }
            "CMakeLists.txt" => {
                builds.insert("cmake");
            }
            _ => {}
        }
    }

    ProjectInfo {
        root: root.to_path_buf(),
        git_root: git_root(root),
        languages,
        manifests,
        package_managers: pms.into_iter().map(Into::into).collect(),
        frameworks: fw.into_iter().map(Into::into).collect(),
        build_systems: builds.into_iter().map(Into::into).collect(),
        test_frameworks: tests.into_iter().map(Into::into).collect(),
        commands: cmds,
    }
}

#[cfg(test)]
mod tests {
    use super::super::index::tests::temp_repo;
    use super::*;

    #[test]
    fn detects_rust_and_node_projects() {
        let root = temp_repo(&[
            ("Cargo.toml", "[package]\nname=\"x\"\n[dependencies]\ntokio = \"1\"\n"),
            ("src/main.rs", "fn main() {}\n"),
            (
                "web/package.json",
                r#"{"scripts":{"build":"vite build","test":"vitest"},"devDependencies":{"vitest":"1","typescript":"5","react":"19"}}"#,
            ),
            ("web/pnpm-lock.yaml", ""),
            ("web/src/a.tsx", "export const A = () => null;\n"),
            ("fixtures/deep/nested/package.json", "{}"),
        ]);
        let mut idx = Index::new(root.clone(), &[]).unwrap();
        idx.refresh();
        let p = detect(&idx);
        assert_eq!(p.manifests, vec!["Cargo.toml", "web/package.json"]);
        assert_eq!(p.package_managers, vec!["cargo", "pnpm"]);
        assert!(p.frameworks.contains(&"tokio".into()) && p.frameworks.contains(&"react".into()));
        assert!(p.test_frameworks.contains(&"vitest".into()));
        assert!(p.commands.test.contains(&cmd(&["pnpm", "run", "test"], "web", "web/package.json")));
        assert_eq!(p.commands.build[0].argv, ["cargo", "build"]);
        assert_eq!(p.languages.iter().map(|l| l.language.as_str()).collect::<Vec<_>>(), ["rust", "typescript"]);
        std::fs::remove_dir_all(root).ok();
    }
}
