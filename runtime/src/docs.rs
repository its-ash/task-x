//! Library docs for `@docs` (`docs/add`, `docs/list`, `docs/remove`, and the `docs_search` tool).
//! A docs source is a start URL: pages under the same host and path are crawled (breadth first,
//! up to a page limit), reduced to text, and cut into chunks. With an embedding model the chunks
//! get vectors; without one, search falls back to keywords. Saved under `<data>/docs/<name>.json`.

use crate::context::semantic::{self, Store};
use crate::provider::Providers;
use crate::provider::types::ProviderConfig;
use crate::tools::web;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

pub const DEFAULT_PAGES: usize = 40;
pub const MAX_PAGES: usize = 300;
/// Pages fetched at the same time.
const PARALLEL: usize = 4;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub name: String,
    pub url: String,
    pub pages: usize,
    pub indexed_ms: u64,
    /// The embedding model, or empty for keyword search.
    #[serde(default)]
    pub model: String,
    pub store: Store,
}

/// Names become file names: letters, digits, `.`, `_`, and `-`.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("docs")
}

fn path(data_dir: &Path, name: &str) -> Option<PathBuf> {
    valid_name(name).then(|| dir(data_dir).join(format!("{name}.json")))
}

pub fn load(data_dir: &Path, name: &str) -> Option<Source> {
    serde_json::from_slice(&std::fs::read(path(data_dir, name)?).ok()?).ok()
}

/// Every source with its size, without the chunks.
pub fn list(data_dir: &Path) -> Value {
    let mut out: Vec<Value> = std::fs::read_dir(dir(data_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(str::to_owned))
        .filter_map(|n| load(data_dir, &n))
        .map(|s| json!({ "name": s.name, "url": s.url, "pages": s.pages, "chunks": s.store.chunks.len(), "indexedMs": s.indexed_ms, "semantic": !s.model.is_empty() }))
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!(out)
}

pub fn remove(data_dir: &Path, name: &str) -> bool {
    path(data_dir, name).is_some_and(|p| std::fs::remove_file(p).is_ok())
}

/// Whether `u` is under the start page: same host, and a path inside the start's folder.
fn in_scope(start: &url::Url, u: &url::Url) -> bool {
    let base = start.path().rsplit_once('/').map_or("/", |(d, _)| d);
    u.host_str() == start.host_str() && u.path().starts_with(base) && !u.path().rsplit('/').next().is_some_and(|f| {
        let f = f.to_ascii_lowercase();
        [".png", ".jpg", ".jpeg", ".gif", ".svg", ".zip", ".gz", ".pdf", ".css", ".js", ".woff", ".woff2", ".ico"].iter().any(|x| f.ends_with(x))
    })
}

/// Crawls and indexes one source, calling `progress(pages done, pages queued)` as it goes.
#[allow(clippy::too_many_arguments)]
pub async fn add(
    providers: &Providers,
    embed: Option<&ProviderConfig>,
    data_dir: &Path,
    name: &str,
    start: &str,
    max_pages: usize,
    cancel: &CancellationToken,
    progress: impl Fn(usize, usize),
) -> Result<Value, String> {
    if !valid_name(name) {
        return Err("the name must be 1-64 letters, digits, '.', '_', or '-'".into());
    }
    let start = url::Url::parse(start.trim()).map_err(|e| format!("invalid URL: {e}"))?;
    let max = max_pages.clamp(1, MAX_PAGES);
    let mut queue: VecDeque<url::Url> = VecDeque::from([start.clone()]);
    let mut seen: HashSet<String> = HashSet::from([start.to_string()]);
    let mut pieces = Vec::new();
    let mut pages = 0;
    let mut errors = Vec::new();
    while !queue.is_empty() && pages < max {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let batch: Vec<url::Url> = (0..PARALLEL.min(max - pages)).filter_map(|_| queue.pop_front()).collect();
        let got = futures_util::future::join_all(batch.iter().map(web::fetch_page)).await;
        for (u, r) in batch.iter().zip(got) {
            match r {
                Ok((final_url, title, text, links)) => {
                    pages += 1;
                    pieces.extend(semantic::doc_pieces(final_url.as_str(), &title, &text));
                    for l in links.into_iter().filter(|l| in_scope(&start, l)) {
                        if seen.len() < max * 20 && seen.insert(l.to_string()) {
                            queue.push_back(l);
                        }
                    }
                }
                Err(e) if pages == 0 && u == &start => return Err(format!("could not read {start}: {}", e.message)),
                Err(e) => errors.push(format!("{u}: {}", e.message)),
            }
        }
        progress(pages, pages + queue.len().min(max - pages));
    }
    if pieces.is_empty() {
        return Err(format!("{start} has no readable text"));
    }
    let mut store = Store::default();
    let model = embed.map(|e| format!("{:?}:{}", e.kind, e.model)).unwrap_or_default();
    let todo = store.plan(&model, pieces, true);
    match embed {
        Some(cfg) => {
            for batch in todo.chunks(semantic::BATCH) {
                let texts: Vec<String> = batch.iter().map(semantic::embed_text).collect();
                let vecs = providers.embed(cfg, &texts, cancel).await.map_err(|e| format!("embedding failed: {}", e.message))?;
                for (p, v) in batch.iter().zip(vecs) {
                    store.add(semantic::Piece { source: p.source.clone(), start: p.start, end: p.end, title: p.title.clone(), text: p.text.clone() }, v, true);
                }
            }
        }
        None => {
            for p in todo {
                store.add(p, Vec::new(), true);
            }
        }
    }
    let chunks = store.chunks.len();
    let src = Source {
        name: name.to_owned(),
        url: start.to_string(),
        pages,
        indexed_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
        model,
        store,
    };
    let p = path(data_dir, name).ok_or("invalid name")?;
    std::fs::create_dir_all(dir(data_dir)).map_err(|e| e.to_string())?;
    std::fs::write(&p, serde_json::to_vec(&src).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(json!({ "name": name, "url": src.url, "pages": pages, "chunks": chunks, "semantic": !src.model.is_empty(), "skipped": errors.len() }))
}

/// The best chunks for `query` in one source or all of them: by embedding when the source has
/// vectors and a model is given, else by keywords.
pub async fn search(
    providers: &Providers,
    embed: Option<&ProviderConfig>,
    data_dir: &Path,
    source: Option<&str>,
    query: &str,
    k: usize,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    let names: Vec<String> = match source {
        Some(s) => vec![s.to_owned()],
        None => list(data_dir).as_array().into_iter().flatten().filter_map(|s| s["name"].as_str().map(str::to_owned)).collect(),
    };
    if names.is_empty() {
        return Err("no docs are indexed yet: add a source with \"task-x: Add Docs…\" (a docs start URL)".into());
    }
    let mut hits: Vec<(f32, Value)> = Vec::new();
    let mut query_vec: Option<(String, Vec<f32>)> = None;
    for name in &names {
        let Some(src) = load(data_dir, name) else { return Err(format!("no docs source named '{name}'")) };
        let semantic = embed.filter(|e| !src.model.is_empty() && src.model == format!("{:?}:{}", e.kind, e.model));
        let found = match semantic {
            Some(cfg) => {
                if query_vec.as_ref().map(|(m, _)| m != &src.model).unwrap_or(true) {
                    let v = providers.embed(cfg, &[query.to_owned()], cancel).await.map_err(|e| e.message)?;
                    query_vec = Some((src.model.clone(), v.into_iter().next().unwrap_or_default()));
                }
                src.store.search(&query_vec.as_ref().expect("query vector").1, k, |_| true)
            }
            None => semantic::keyword_search(&src.store.chunks, query, k, |_| true),
        };
        hits.extend(found.into_iter().map(|(s, c)| (s, json!({ "source": name, "url": c.source, "title": c.title, "score": (s * 1000.0).round() / 1000.0, "text": c.text }))));
    }
    hits.sort_by(|a, b| b.0.total_cmp(&a.0));
    Ok(json!({ "query": query, "results": hits.into_iter().take(k).map(|(_, v)| v).collect::<Vec<_>>() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pages_under_the_start_folder_are_crawled() {
        let start = url::Url::parse("https://docs.rs/serde/latest/serde/index.html").unwrap();
        let ok = |u: &str| in_scope(&start, &url::Url::parse(u).unwrap());
        assert!(ok("https://docs.rs/serde/latest/serde/de/index.html"));
        assert!(!ok("https://docs.rs/tokio/latest/tokio/"), "another crate");
        assert!(!ok("https://example.com/serde/latest/serde/x.html"), "another host");
        assert!(!ok("https://docs.rs/serde/latest/serde/logo.png"), "not a page");
    }

    #[test]
    fn sources_are_listed_searched_and_removed() {
        let dir = std::env::temp_dir().join(format!("task-x-docs-{}", std::process::id()));
        let mut store = Store::default();
        for p in semantic::doc_pieces("https://x.dev/a", "Client", "Build a client with Client::builder().\n\nTimeouts are set per request.") {
            store.add(p, Vec::new(), true);
        }
        let src = Source { name: "x".into(), url: "https://x.dev/".into(), pages: 1, indexed_ms: 1, model: String::new(), store };
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs").join("x.json"), serde_json::to_vec(&src).unwrap()).unwrap();
        assert_eq!(list(&dir)[0]["name"], "x");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let providers = Providers::new().unwrap();
        let r = rt.block_on(search(&providers, None, &dir, Some("x"), "client builder", 3, &CancellationToken::new())).unwrap();
        assert!(r["results"][0]["text"].as_str().unwrap().contains("Client::builder"));
        assert!(remove(&dir, "x") && !remove(&dir, "x"));
        assert!(!valid_name("../x") && !remove(&dir, "../x"));
        std::fs::remove_dir_all(dir).ok();
    }
}
