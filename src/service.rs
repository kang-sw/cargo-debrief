use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// /// Prints current RSS (from `ps`) to stderr for memory profiling.
// fn log_rss(label: &str) {
//     let pid = std::process::id();
//     let output = std::process::Command::new("ps")
//         .args(["-o", "rss=", "-p", &pid.to_string()])
//         .output();
//     match output {
//         Ok(out) => {
//             let raw = String::from_utf8_lossy(&out.stdout);
//             match raw.trim().parse::<u64>() {
//                 Ok(kb) => eprintln!("[RSS] {}: {:.1} MB", label, kb as f64 / 1024.0),
//                 Err(_) => eprintln!("[RSS] {}: (unavailable)", label),
//             }
//         }
//         Err(_) => eprintln!("[RSS] {}: (unavailable)", label),
//     }
// }

use crate::{
    chunk::{Chunk, ChunkOrigin, ChunkType, Visibility},
    chunker::{Chunker, RustChunker},
    config::{config_paths, load_config, save_config},
    deps,
    embedder::{Embedder, ModelRegistry},
    git,
    search::SearchIndex,
    store::{self, DepsIndexData, IndexData},
};

/// Result of an indexing operation.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IndexResult {
    pub files_indexed: usize,
    pub chunks_created: usize,
}

/// A single search result with relevance score.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub file_path: String,
    pub line_range: (usize, usize),
    pub score: f64,
    pub display_text: String,
    pub module_path: String,
    pub origin: ChunkOrigin,
}

/// Service boundary between CLI and core logic.
///
/// Each method receives `project_root` explicitly, enabling a single
/// daemon instance to serve multiple workspaces without a separate
/// routing/multiplexing layer.
///
/// Phase 1: [`InProcessService`] executes everything in-process.
/// Phase 2: `DaemonClient` will send requests over IPC to a background daemon,
/// including `project_root` in each request.
///
/// Not object-safe (uses RPITIT). Dispatch is monomorphized at compile time.
pub trait DebriefService {
    fn index(
        &self,
        project_root: &Path,
        path: &Path,
    ) -> impl Future<Output = Result<IndexResult>> + Send;

    fn search(
        &self,
        project_root: &Path,
        query: &str,
        top_k: usize,
        include_deps: bool,
    ) -> impl Future<Output = Result<Vec<SearchResult>>> + Send;

    fn overview(
        &self,
        project_root: &Path,
        file: &Path,
    ) -> impl Future<Output = Result<String>> + Send;

    fn dep_overview(
        &self,
        project_root: &Path,
        crate_name: &str,
    ) -> impl Future<Output = Result<String>> + Send;

    fn set_embedding_model(
        &self,
        project_root: &Path,
        model: &str,
        global: bool,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// In-process service implementation. Executes all operations directly
/// in the CLI process without a daemon. Config is resolved from the
/// `project_root` on each call.
pub struct InProcessService;

impl InProcessService {
    pub fn new() -> Self {
        Self
    }
}

impl Default for InProcessService {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Private pipeline helpers
// ---------------------------------------------------------------------------

/// Compute the index file path: `.git/debrief/index.bin` relative to the git root.
///
/// Walks parent directories to find `.git/`. Acceptable duplication of the
/// logic in `config.rs::find_git_root`; refactor if a third caller appears.
fn index_path(project_root: &Path) -> Result<PathBuf> {
    let mut current = project_root;
    loop {
        let candidate = current.join(".git");
        if candidate.is_dir() {
            return Ok(candidate.join("debrief").join("index.bin"));
        }
        current = current
            .parent()
            .context("not inside a git repository; cannot locate index path")?;
    }
}

/// Construct an `Embedder` for the given model name, downloading if needed.
async fn make_embedder(model_name: &str) -> Result<Embedder> {
    let cache_dir = dirs::data_dir()
        .context("cannot determine user data directory (no home directory?)")?
        .join("debrief")
        .join("models");
    Embedder::load(model_name, &cache_dir).await
}

/// Ensure the on-disk index is up-to-date with the current HEAD and configured model.
///
/// - No index on disk → full reindex.
/// - Stored model != configured model → full reindex.
/// - Stored commit != HEAD → incremental reindex.
/// - Already fresh → return stored index unchanged.
///
/// Called silently before `search` and `overview`; no user-visible output.
async fn ensure_index_fresh(project_root: &Path, embedder: &Embedder) -> Result<IndexData> {
    let paths = config_paths(project_root);
    let config = load_config(&paths)?;
    let model_name = config
        .embedding_model
        .as_deref()
        .unwrap_or(ModelRegistry::DEFAULT_MODEL);

    let idx_path = index_path(project_root)?;
    let existing = store::load_index(&idx_path)?;
    let head = git::head_commit(project_root)?;

    let (prior_commit, base_index) = match existing {
        None => (None, None),
        Some(ref data) if data.embedding_model.as_deref() != Some(model_name) => (None, None),
        Some(ref data) if data.last_indexed_commit.as_deref() != Some(head.as_str()) => {
            let prior = data.last_indexed_commit.clone();
            (prior, Some(existing.unwrap()))
        }
        Some(data) => return Ok(data),
    };

    let (data, _) = run_index(project_root, prior_commit.as_deref(), base_index, embedder).await?;
    Ok(data)
}

/// Core indexing pipeline.
///
/// `prior_commit = None` triggers a full reindex via `git ls-files`.
/// `prior_commit = Some(hash)` triggers an incremental reindex diffing `hash..HEAD`.
async fn run_index(
    project_root: &Path,
    prior_commit: Option<&str>,
    existing_index: Option<IndexData>,
    embedder: &Embedder,
) -> Result<(IndexData, IndexResult)> {
    let paths = config_paths(project_root);
    let config = load_config(&paths)?;
    let model_name = config
        .embedding_model
        .as_deref()
        .unwrap_or(ModelRegistry::DEFAULT_MODEL);

    let changes = git::changed_files(project_root, prior_commit)?;

    let mut base_index = existing_index.unwrap_or_else(IndexData::new);

    // Remove deleted files from the index.
    for deleted_path in &changes.deleted {
        base_index.chunks.remove(&PathBuf::from(deleted_path));
    }

    // Collect .rs files to embed from added + modified.
    let files_to_embed: Vec<String> = changes
        .added
        .iter()
        .chain(changes.modified.iter())
        .filter(|p| p.ends_with(".rs"))
        .cloned()
        .collect();

    // Chunk all files and collect embedding texts.
    let chunker = RustChunker;
    let mut per_file_chunks: Vec<(String, Vec<Chunk>)> = Vec::new();
    let mut all_embedding_texts: Vec<String> = Vec::new();

    for rel_path in &files_to_embed {
        let abs_path = project_root.join(rel_path);
        let source = std::fs::read_to_string(&abs_path)
            .with_context(|| format!("failed to read {}", abs_path.display()))?;
        let chunks = chunker
            .chunk(Path::new(rel_path), &source)
            .with_context(|| format!("failed to chunk {rel_path}"))?;

        for c in &chunks {
            all_embedding_texts.push(c.embedding_text.clone());
        }
        per_file_chunks.push((rel_path.clone(), chunks));
    }

    // Embed texts in fixed-size batches to bound peak memory.
    const EMBED_BATCH_SIZE: usize = 64;
    let embeddings: Vec<Vec<f32>> = if all_embedding_texts.is_empty() {
        vec![]
    } else {
        use std::io::Write;
        eprint!("indexing");
        let _ = std::io::stderr().flush();

        let mut accumulated = Vec::with_capacity(all_embedding_texts.len());
        for batch in all_embedding_texts.chunks(EMBED_BATCH_SIZE) {
            let text_refs: Vec<&str> = batch.iter().map(|s| s.as_str()).collect();
            accumulated.extend(embedder.embed_batch(&text_refs)?);
            eprint!(".");
            let _ = std::io::stderr().flush();
        }
        eprintln!(
            "\ndone. {} chunks, {} files.",
            all_embedding_texts.len(),
            files_to_embed.len()
        );
        accumulated
    };

    // Assign embeddings back to chunks by position and insert into the index.
    assert_eq!(
        embeddings.len(),
        all_embedding_texts.len(),
        "embed_batch returned {} vectors, expected {}",
        embeddings.len(),
        all_embedding_texts.len()
    );
    let mut emb_idx = 0usize;
    let mut chunks_created = 0usize;

    for (rel_path, mut chunks) in per_file_chunks {
        for chunk in &mut chunks {
            chunk.embedding = Some(embeddings[emb_idx].clone());
            emb_idx += 1;
        }
        chunks_created += chunks.len();
        base_index.chunks.insert(PathBuf::from(&rel_path), chunks);
    }

    let files_indexed = files_to_embed.len();
    base_index.last_indexed_commit = Some(git::head_commit(project_root)?);
    base_index.embedding_model = Some(model_name.to_string());

    let idx_path = index_path(project_root)?;
    store::save_index(&idx_path, &base_index)?;

    Ok((
        base_index,
        IndexResult {
            files_indexed,
            chunks_created,
        },
    ))
}

// ---------------------------------------------------------------------------
// Dependency indexing helpers
// ---------------------------------------------------------------------------

/// Compute the dependency index path: `.git/debrief/deps-index.bin`.
fn deps_index_path(project_root: &Path) -> Result<PathBuf> {
    let mut current = project_root;
    loop {
        let candidate = current.join(".git");
        if candidate.is_dir() {
            return Ok(candidate.join("debrief").join("deps-index.bin"));
        }
        current = current
            .parent()
            .context("not inside a git repository; cannot locate deps index path")?;
    }
}

/// Hash the contents of `Cargo.lock` using `DefaultHasher`.
///
/// Output is deterministic within a process. A hash collision or Rust version
/// change triggers a harmless full dep reindex — acceptable without adding a
/// `sha2`/`blake3` dependency.
fn cargo_lock_content_hash(project_root: &Path) -> Result<String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let bytes = std::fs::read(project_root.join("Cargo.lock")).context("Cargo.lock not found")?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

/// Recursively collect `.rs` files under `src_dir`.
///
/// Returns an empty vec (no error) when `src_dir` does not exist — some
/// crates in the registry cache have no `src/` directory.
fn collect_dep_rs_files(src_dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    collect_rs_recursive(src_dir, &mut result);
    result
}

fn collect_rs_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_recursive(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Build the annotated embedding text for a dependency chunk.
///
/// Prepends `[dependency] {crate_name} (dependency of: {root_deps, ...})`
/// or `[dependency] {crate_name}` when `root_deps` is empty.
fn build_dep_embedding_text(
    crate_name: &str,
    root_deps: &[String],
    original_embedding_text: &str,
) -> String {
    let header = if root_deps.is_empty() {
        format!("[dependency] {crate_name}")
    } else {
        format!(
            "[dependency] {crate_name} (dependency of: {})",
            root_deps.join(", ")
        )
    };
    format!("{header}\n{original_embedding_text}")
}

/// Index all non-workspace dependency packages and write `deps-index.bin`.
///
/// Walks `dep.src_root/src/` for each package, chunks `.rs` files with
/// `RustChunker`, retains only `pub` items, annotates embedding text, and
/// embeds in batches.
async fn run_deps_index(project_root: &Path, embedder: &Embedder) -> Result<DepsIndexData> {
    let config = load_config(&config_paths(project_root))?;
    // eprintln!("[deps] discovering dependency packages via cargo metadata...");
    let mut packages = deps::discover_dependency_packages(project_root)?;
    let lock_hash = cargo_lock_content_hash(project_root).ok();

    // Apply exclude list from config before chunking.
    let exclude: std::collections::HashSet<&str> = config
        .dependencies
        .as_ref()
        .and_then(|d| d.exclude.as_ref())
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();

    if !exclude.is_empty() {
        packages.retain(|p| !exclude.contains(p.crate_name.as_str()));
    }

    // eprintln!("[deps] found {} dependency packages (after exclude filter)", packages.len());
    // log_rss("after cargo metadata");

    let chunker = RustChunker;
    let mut all_chunks: Vec<Chunk> = Vec::new();
    let mut all_embedding_texts: Vec<String> = Vec::new();

    for (pkg_index, package) in packages.iter().enumerate() {
        let _ = pkg_index; // used by diagnostic logging below
        // eprintln!("[deps] [{}/{}] chunking {} (src: {})",
        //     pkg_index + 1, packages.len(), package.crate_name, package.src_root.display());
        let src_dir = package.src_root.join("src");
        for rs_file in collect_dep_rs_files(&src_dir) {
            let relative = rs_file
                .strip_prefix(&package.src_root)
                .unwrap_or(&rs_file)
                .to_path_buf();
            let source = match std::fs::read_to_string(&rs_file) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let raw_chunks = match chunker.chunk(&relative, &source) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for mut chunk in raw_chunks {
                if chunk.metadata.visibility != Visibility::Pub {
                    continue;
                }
                let annotated = build_dep_embedding_text(
                    &package.crate_name,
                    &package.root_deps,
                    &chunk.embedding_text,
                );
                chunk.embedding_text = annotated.clone();
                chunk.origin = ChunkOrigin::Dependency {
                    crate_name: package.crate_name.clone(),
                    crate_version: package.crate_version.clone(),
                    root_deps: package.root_deps.clone(),
                };
                all_embedding_texts.push(annotated);
                all_chunks.push(chunk);
            }
        }
    }

    // eprintln!("[deps] collection complete: {} chunks from {} packages",
    //     all_chunks.len(), packages.len());
    // log_rss("after chunk collection");

    // Embed in batches.
    const EMBED_BATCH_SIZE: usize = 64;
    let embeddings: Vec<Vec<f32>> = if all_embedding_texts.is_empty() {
        vec![]
    } else {
        use std::io::Write;
        let total_batches = (all_embedding_texts.len() + EMBED_BATCH_SIZE - 1) / EMBED_BATCH_SIZE;
        // eprintln!("[deps] starting embedding ({} batches of max {})",
        //     total_batches, EMBED_BATCH_SIZE);
        eprint!("indexing deps");
        let _ = std::io::stderr().flush();

        let mut accumulated = Vec::with_capacity(all_embedding_texts.len());
        for (batch_idx, batch) in all_embedding_texts.chunks(EMBED_BATCH_SIZE).enumerate() {
            let _ = (batch_idx, total_batches); // used by diagnostic logging below
            // eprintln!("[deps] batch {}/{}: embedding chunks {}-{}",
            //     batch_idx + 1, total_batches,
            //     batch_idx * EMBED_BATCH_SIZE,
            //     (batch_idx * EMBED_BATCH_SIZE + batch.len()).min(all_embedding_texts.len()));
            // if batch_idx % 10 == 0 {
            //     log_rss(&format!("embedding batch {}", batch_idx + 1));
            // }
            let text_refs: Vec<&str> = batch.iter().map(|s| s.as_str()).collect();
            accumulated.extend(embedder.embed_batch(&text_refs)?);
            eprint!(".");
            let _ = std::io::stderr().flush();
        }
        eprintln!("\ndone. {} dep chunks.", all_embedding_texts.len());
        accumulated
    };

    assert_eq!(
        embeddings.len(),
        all_chunks.len(),
        "embed_batch returned {} vectors for {} dep chunks",
        embeddings.len(),
        all_chunks.len()
    );

    for (chunk, emb) in all_chunks.iter_mut().zip(embeddings) {
        chunk.embedding = Some(emb);
    }

    let mut data = DepsIndexData::new();
    data.cargo_lock_hash = lock_hash;
    data.chunks = all_chunks;

    let path = deps_index_path(project_root)?;
    store::save_deps_index(&path, &data)?;
    Ok(data)
}

/// Ensure the dep index is fresh relative to `Cargo.lock`.
///
/// If the stored hash matches the current `Cargo.lock` hash → return cached data.
/// Otherwise → run a full dep reindex.
async fn ensure_deps_index_fresh(
    project_root: &Path,
    embedder: &Embedder,
) -> Result<DepsIndexData> {
    let current_hash = cargo_lock_content_hash(project_root).ok();
    let path = deps_index_path(project_root)?;

    if let Some(data) = store::load_deps_index(&path)?
        && data.cargo_lock_hash == current_hash
        && current_hash.is_some()
    {
        return Ok(data);
    }

    run_deps_index(project_root, embedder).await
}

// ---------------------------------------------------------------------------
// DebriefService implementation
// ---------------------------------------------------------------------------

impl DebriefService for InProcessService {
    async fn index(&self, project_root: &Path, _path: &Path) -> Result<IndexResult> {
        let config = load_config(&config_paths(project_root))?;
        let model_name = config
            .embedding_model
            .as_deref()
            .unwrap_or(ModelRegistry::DEFAULT_MODEL);
        let embedder = make_embedder(model_name).await?;

        // Full reindex: no prior commit, no existing index.
        // eprintln!("[index] starting project index...");
        let (_data, result) = run_index(project_root, None, None, &embedder).await?;
        // eprintln!("[index] project index done. Starting deps index...");
        run_deps_index(project_root, &embedder).await?;
        // eprintln!("[index] deps index done.");
        Ok(result)
    }

    async fn search(
        &self,
        project_root: &Path,
        query: &str,
        top_k: usize,
        include_deps: bool,
    ) -> Result<Vec<SearchResult>> {
        let config = load_config(&config_paths(project_root))?;
        let model_name = config
            .embedding_model
            .as_deref()
            .unwrap_or(ModelRegistry::DEFAULT_MODEL);
        let embedder = make_embedder(model_name).await?;

        let index_data = ensure_index_fresh(project_root, &embedder).await?;

        let mut flat_chunks: Vec<(PathBuf, Chunk)> = index_data
            .chunks
            .into_iter()
            .flat_map(|(path, chunks)| chunks.into_iter().map(move |c| (path.clone(), c)))
            .collect();

        if include_deps {
            let deps_data = ensure_deps_index_fresh(project_root, &embedder).await?;
            for chunk in deps_data.chunks {
                flat_chunks.push((PathBuf::from(&chunk.metadata.file_path), chunk));
            }
        }

        let search_index = SearchIndex::build(flat_chunks)?;
        search_index.search(query, &embedder, top_k)
    }

    async fn overview(&self, project_root: &Path, file: &Path) -> Result<String> {
        let config = load_config(&config_paths(project_root))?;
        let model_name = config
            .embedding_model
            .as_deref()
            .unwrap_or(ModelRegistry::DEFAULT_MODEL);
        let embedder = make_embedder(model_name).await?;
        let index_data = ensure_index_fresh(project_root, &embedder).await?;

        // Normalize `file` to repo-relative form for index key lookup.
        let file_key = if file.is_absolute() {
            file.strip_prefix(project_root)
                .with_context(|| {
                    format!("{} is not under {}", file.display(), project_root.display())
                })?
                .to_path_buf()
        } else {
            file.to_path_buf()
        };

        let chunks = index_data
            .chunks
            .get(&file_key)
            .ok_or_else(|| anyhow::anyhow!("no index entry for {}", file.display()))?;

        let mut overview_chunks: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| c.metadata.chunk_type == ChunkType::Overview)
            .collect();

        overview_chunks.sort_by_key(|c| match c.metadata.visibility {
            Visibility::Pub => 0,
            Visibility::PubCrate => 1,
            Visibility::PubSuper => 2,
            Visibility::Private => 3,
        });

        let overview_text: String = overview_chunks
            .iter()
            .map(|c| c.display_text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        if overview_text.is_empty() {
            anyhow::bail!("no overview chunks found for {}", file.display());
        }
        Ok(overview_text)
    }

    async fn dep_overview(&self, project_root: &Path, crate_name: &str) -> Result<String> {
        let dep_index_path = deps_index_path(project_root)?;
        let data = store::load_deps_index(&dep_index_path)?
            .ok_or_else(|| anyhow::anyhow!("no dependency index; run `rebuild-index` first"))?;

        let mut overview_chunks: Vec<&Chunk> = data
            .chunks
            .iter()
            .filter(|c| {
                matches!(&c.origin, ChunkOrigin::Dependency { crate_name: cn, .. } if cn == crate_name)
            })
            .filter(|c| c.metadata.chunk_type == ChunkType::Overview)
            .collect();

        if overview_chunks.is_empty() {
            anyhow::bail!("no overview chunks found for dependency {crate_name:?}");
        }

        overview_chunks.sort_by_key(|c| match c.metadata.visibility {
            Visibility::Pub => 0,
            Visibility::PubCrate => 1,
            Visibility::PubSuper => 2,
            Visibility::Private => 3,
        });

        Ok(overview_chunks
            .iter()
            .map(|c| c.display_text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    async fn set_embedding_model(
        &self,
        project_root: &Path,
        model: &str,
        global: bool,
    ) -> Result<()> {
        ModelRegistry::lookup(model).with_context(|| {
            format!(
                "unknown embedding model: {model:?}. Use a known model name such as {:?}",
                ModelRegistry::DEFAULT_MODEL
            )
        })?;

        let paths = config_paths(project_root);

        if global {
            let target = paths
                .global
                .context("could not determine global config path (no home directory?)")?;
            let mut config = crate::config::load_layer_single(&target)?.unwrap_or_default();
            config.embedding_model = Some(model.to_string());
            save_config(&target, &config)?;
        } else {
            let target = paths
                .project
                .context("not inside a git repository; cannot write project config")?;
            let mut config = crate::config::load_layer_single(&target)?.unwrap_or_default();
            config.embedding_model = Some(model.to_string());
            save_config(&target, &config)?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DaemonClient — IPC-based service implementation
// ---------------------------------------------------------------------------

/// Client that communicates with a running daemon process via IPC.
/// Implements `DebriefService` by serializing requests and deserializing responses.
pub struct DaemonClient {
    daemon_dir: std::path::PathBuf,
    timeout: std::time::Duration,
}

impl DaemonClient {
    /// Connect to an existing daemon for the given project root.
    /// Returns `None` if no daemon is running.
    pub fn connect(project_root: &Path) -> Option<Self> {
        let dir = crate::daemon::daemon_dir(project_root).ok()?;
        if !crate::daemon::is_daemon_running(&dir) {
            return None;
        }

        // Debug builds: check binary identity to detect stale daemon after recompile.
        if !crate::daemon::check_binary_identity(&dir) {
            eprintln!("[daemon] binary mismatch, killing stale daemon");
            crate::daemon::kill_stale_daemon(&dir);
            return None;
        }

        let ready = crate::ipc::ready_indicator(&dir);
        if !ready.exists() {
            return None;
        }

        Some(Self {
            daemon_dir: dir,
            timeout: std::time::Duration::from_secs(120),
        })
    }

    fn send(
        &self,
        request: crate::ipc::protocol::DaemonRequest,
    ) -> Result<crate::ipc::protocol::DaemonResponse> {
        crate::ipc::send_command(&self.daemon_dir, request, self.timeout)
    }
}

impl DebriefService for DaemonClient {
    async fn index(&self, _project_root: &Path, path: &Path) -> Result<IndexResult> {
        use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
        match self.send(DaemonRequest::Index {
            path: path.to_path_buf(),
        })? {
            DaemonResponse::IndexResult(r) => Ok(r),
            DaemonResponse::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected response: {other:?}"),
        }
    }

    async fn search(
        &self,
        _project_root: &Path,
        query: &str,
        top_k: usize,
        include_deps: bool,
    ) -> Result<Vec<SearchResult>> {
        use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
        match self.send(DaemonRequest::Search {
            query: query.to_string(),
            top_k,
            include_deps,
        })? {
            DaemonResponse::SearchResults { results } => Ok(results),
            DaemonResponse::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected response: {other:?}"),
        }
    }

    async fn overview(&self, _project_root: &Path, file: &Path) -> Result<String> {
        use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
        match self.send(DaemonRequest::Overview {
            file: file.to_path_buf(),
        })? {
            DaemonResponse::Overview { content } => Ok(content),
            DaemonResponse::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected response: {other:?}"),
        }
    }

    async fn dep_overview(&self, _project_root: &Path, crate_name: &str) -> Result<String> {
        use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
        match self.send(DaemonRequest::DepOverview {
            crate_name: crate_name.to_string(),
        })? {
            DaemonResponse::Overview { content } => Ok(content),
            DaemonResponse::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected response: {other:?}"),
        }
    }

    async fn set_embedding_model(
        &self,
        _project_root: &Path,
        model: &str,
        global: bool,
    ) -> Result<()> {
        use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
        match self.send(DaemonRequest::SetEmbeddingModel {
            model: model.to_string(),
            global,
        })? {
            DaemonResponse::Ok { .. } => Ok(()),
            DaemonResponse::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected response: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Service — dispatch with daemon-first fallback to InProcess
// ---------------------------------------------------------------------------

/// Unified service dispatch: tries daemon first, falls back to in-process
/// on any IPC transport failure (R6/R8/R10).
pub struct Service {
    daemon: Option<DaemonClient>,
    in_process: InProcessService,
}

impl Service {
    /// Create a service with both daemon and in-process backends.
    /// If daemon is None, all requests go directly to in-process.
    pub fn new(daemon: Option<DaemonClient>) -> Self {
        Self {
            daemon,
            in_process: InProcessService::new(),
        }
    }
}

impl DebriefService for Service {
    async fn index(&self, project_root: &Path, path: &Path) -> Result<IndexResult> {
        if let Some(d) = &self.daemon {
            match d.index(project_root, path).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process.index(project_root, path).await
    }

    async fn search(
        &self,
        project_root: &Path,
        query: &str,
        top_k: usize,
        include_deps: bool,
    ) -> Result<Vec<SearchResult>> {
        if let Some(d) = &self.daemon {
            match d.search(project_root, query, top_k, include_deps).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process
            .search(project_root, query, top_k, include_deps)
            .await
    }

    async fn overview(&self, project_root: &Path, file: &Path) -> Result<String> {
        if let Some(d) = &self.daemon {
            match d.overview(project_root, file).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process.overview(project_root, file).await
    }

    async fn dep_overview(&self, project_root: &Path, crate_name: &str) -> Result<String> {
        if let Some(d) = &self.daemon {
            match d.dep_overview(project_root, crate_name).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process.dep_overview(project_root, crate_name).await
    }

    async fn set_embedding_model(
        &self,
        project_root: &Path,
        model: &str,
        global: bool,
    ) -> Result<()> {
        if let Some(d) = &self.daemon {
            match d.set_embedding_model(project_root, model, global).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process
            .set_embedding_model(project_root, model, global)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_embedding_model_rejects_unknown_model() {
        let service = InProcessService::new();
        let root = Path::new(".");

        let err = service
            .set_embedding_model(root, "not-a-real-model", false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown embedding model"),
            "expected unknown model error, got: {err}"
        );
    }

    #[test]
    fn test_cargo_lock_content_hash_is_deterministic() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let h1 = cargo_lock_content_hash(manifest_dir).expect("hash failed");
        let h2 = cargo_lock_content_hash(manifest_dir).expect("hash failed");
        assert_eq!(h1, h2, "hash should be deterministic");
        assert_eq!(h1.len(), 16, "hash should be 16 hex characters");
        assert!(
            h1.chars().all(|c| c.is_ascii_hexdigit()),
            "hash should be hex"
        );
    }

    #[test]
    fn test_collect_dep_rs_files_on_self_src() {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = collect_dep_rs_files(&src_dir);
        assert!(!files.is_empty(), "expected .rs files in src/");
        for f in &files {
            assert_eq!(
                f.extension().and_then(|e| e.to_str()),
                Some("rs"),
                "expected .rs extension, got: {:?}",
                f
            );
        }
    }

    #[test]
    fn test_collect_dep_rs_files_missing_dir() {
        let missing = Path::new("/nonexistent/path/that/does/not/exist");
        let files = collect_dep_rs_files(missing);
        assert!(files.is_empty(), "missing dir should return empty vec");
    }

    #[test]
    fn test_build_dep_embedding_text_with_root_deps() {
        let result = build_dep_embedding_text("serde", &["serde".to_string()], "original text");
        assert!(
            result.starts_with("[dependency] serde (dependency of: serde)"),
            "unexpected: {result}"
        );
        assert!(result.contains("original text"));
    }

    #[test]
    fn test_build_dep_embedding_text_empty_root_deps() {
        let result = build_dep_embedding_text("serde", &[], "original text");
        assert!(
            result.starts_with("[dependency] serde"),
            "unexpected: {result}"
        );
        assert!(
            !result.contains("(dependency of:"),
            "should not contain dependency of clause"
        );
        assert!(result.contains("original text"));
    }

    /// Requires the ONNX model to be cached (~130 MB download on first run).
    #[tokio::test]
    #[ignore]
    async fn test_run_deps_index_on_self() -> anyhow::Result<()> {
        use crate::chunk::ChunkOrigin;

        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let model_name = ModelRegistry::DEFAULT_MODEL;
        let cache_dir = dirs::data_dir()
            .expect("no data dir")
            .join("debrief")
            .join("models");
        let embedder = Embedder::load(model_name, &cache_dir).await?;

        let data = run_deps_index(manifest_dir, &embedder).await?;

        assert!(!data.chunks.is_empty(), "expected dep chunks");

        let dep_chunk = data
            .chunks
            .iter()
            .find(|c| matches!(&c.origin, ChunkOrigin::Dependency { .. }));
        assert!(
            dep_chunk.is_some(),
            "expected at least one Dependency origin"
        );

        for chunk in &data.chunks {
            assert!(chunk.embedding.is_some(), "all chunks should be embedded");
            assert_eq!(
                chunk.metadata.visibility,
                Visibility::Pub,
                "only pub chunks should be included"
            );
            assert!(
                chunk.embedding_text.starts_with("[dependency]"),
                "embedding_text should start with [dependency]"
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn set_embedding_model_project_scope() -> anyhow::Result<()> {
        use std::process::Command;
        use tempfile::tempdir;

        let dir = tempdir()?;
        // Initialize a git repo so config_paths can find a project path.
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()?;

        let service = InProcessService::new();
        service
            .set_embedding_model(dir.path(), "nomic-embed-text-v1.5", false)
            .await?;

        let config_path = dir.path().join(".debrief").join("config.toml");
        assert!(config_path.exists(), "project config should be created");

        let paths = crate::config::ConfigPaths {
            global: None,
            project: Some(config_path),
            local: None,
        };
        let config = crate::config::load_config(&paths)?;
        assert_eq!(
            config.embedding_model.as_deref(),
            Some("nomic-embed-text-v1.5"),
            "embedding model should be written to project config"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_dep_overview_empty_index_returns_error() {
        use std::process::Command;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let service = InProcessService::new();
        let err = service.dep_overview(dir.path(), "serde").await.unwrap_err();
        assert!(
            err.to_string().contains("no dependency index")
                || err.to_string().contains("no overview chunks"),
            "expected dep index missing error, got: {err}"
        );
    }
}
