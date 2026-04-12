//! Service boundary between CLI and core logic.
//!
//! Phase 1 of the multi-language sources epic
//! (`260409-epic-multi-language-sources`). The indexing pipeline is now
//! driven by the `[[sources]]` list resolved from `config.toml` (or the
//! Cargo.toml backward-compat fallback). The legacy
//! `run_index`/`run_deps_index`/`ensure_*_fresh` helpers are gone — they
//! are replaced by `load_or_rebuild_index` and `run_index_for_sources`.
//!
//! Behavioral notes:
//! - Phase 1 stores a single merged project index containing chunks from
//!   every `dep == false` source. `dep == true` entries are dropped with
//!   a `tracing::warn!` and re-introduced in Phase 3.
//! - Incremental rebuilds are intentionally absent in Phase 1: any
//!   commit-hash mismatch triggers a full rebuild from scratch. This
//!   sidesteps the bookkeeping needed when the resolved source set
//!   itself changes between runs.
//! - `--no-deps` and the `include_deps` parameter are kept for trait
//!   stability but become no-ops, logged at `tracing::debug!`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, info_span, warn};

use crate::chunk::{Chunk, ChunkOrigin, ChunkType, Visibility};
use crate::chunker::chunker_for;
use crate::config::{self, Language, SourceEntry, config_paths, load_config, save_config};
use crate::embedder::{Embedder, ModelRegistry};
use crate::git;
use crate::search::SearchIndex;
use crate::store::{self, IndexData};

/// Embedding batch size — must match the historical pipeline so peak
/// memory and progress dot rate stay comparable.
const EMBED_BATCH_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Service boundary between CLI and core logic.
///
/// Each method receives `project_root` explicitly, enabling a single
/// daemon instance to serve multiple workspaces.
///
/// Not object-safe (uses RPITIT). Dispatch is monomorphized at compile time.
pub trait DebriefService {
    // --- Indexing / retrieval (existing shape, preserved) -----------------

    fn index(
        &self,
        project_root: &Path,
        path: &Path,
        include_deps: bool,
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

    // --- Source registration (new) -----------------------------------------

    /// Append a source entry to the project config.
    fn add_source(
        &self,
        project_root: &Path,
        language: Language,
        root: &Path,
        dep: bool,
    ) -> impl Future<Output = Result<()>> + Send;

    /// List registered sources for the project, applying the
    /// Cargo.toml backward-compat fallback when the config has none.
    fn list_sources(
        &self,
        project_root: &Path,
    ) -> impl Future<Output = Result<Vec<SourceEntry>>> + Send;

    /// Remove the source entry at `index` from the project config.
    fn remove_source(
        &self,
        project_root: &Path,
        index: usize,
    ) -> impl Future<Output = Result<()>> + Send;
}

// ---------------------------------------------------------------------------
// Pipeline helpers (private free functions)
// ---------------------------------------------------------------------------

/// Compute the index file path: `.git/debrief/index.bin` under the git
/// root. Walks parents like `daemon::daemon_dir`.
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

/// Construct an `Embedder` for the project's configured (or default)
/// model. Returns the embedder and the canonical model name string —
/// callers stamp the model name onto `IndexData::embedding_model` so
/// staleness checks can detect future model changes.
async fn build_embedder(project_root: &Path) -> Result<(Embedder, String)> {
    let config = load_config(&config_paths(project_root))?;
    let model_name = config
        .embedding_model
        .as_deref()
        .unwrap_or(ModelRegistry::DEFAULT_MODEL)
        .to_string();
    // Validate the name resolves to a known model spec before downloading.
    ModelRegistry::lookup(&model_name)
        .with_context(|| format!("unknown embedding model in config: {model_name:?}"))?;

    let cache_dir = dirs::data_dir()
        .context("cannot determine user data directory (no home directory?)")?
        .join("debrief")
        .join("models");

    let embedder = Embedder::load(&model_name, &cache_dir).await?;
    Ok((embedder, model_name))
}

/// Default extension set for a language. Returned slice is `&'static`.
fn default_extensions(language: Language) -> &'static [&'static str] {
    match language {
        Language::Rust => &["rs"],
        Language::Cpp => &["cpp", "cc", "cxx", "c", "h", "hpp", "hxx", "hh"],
    }
}

/// Lexically join `project_root` with `entry.root`, treating `"."` as
/// `project_root` itself. Returns the project-relative root used as a
/// prefix filter for git ls-files output.
///
/// Phase 1 keeps this purely lexical (no `canonicalize`) so the function
/// is symlink-stable and unit-testable without filesystem access.
fn entry_relative_root(entry_root: &Path) -> PathBuf {
    if entry_root.as_os_str() == "." || entry_root.as_os_str().is_empty() {
        PathBuf::new()
    } else {
        // Strip a leading `./` if present to keep prefix matches simple.
        let stripped = entry_root.strip_prefix(".").unwrap_or(entry_root);
        stripped.to_path_buf()
    }
}

/// Test whether `relative` (a repo-relative path from `git ls-files`)
/// lies under `root` (also repo-relative). An empty `root` matches every
/// path. Otherwise the test is a path-component prefix match.
fn path_under_root(relative: &Path, root: &Path) -> bool {
    if root.as_os_str().is_empty() {
        return true;
    }
    relative.starts_with(root)
}

/// Return the lowercased file extension of `path`, or `None` if absent.
fn lowercase_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
}

/// Resolve the on-disk index, performing a full rebuild whenever it is
/// missing or stale.
///
/// Phase 1 staleness rules (binary; no incremental diffs):
/// - Missing file → rebuild.
/// - `embedding_model` differs from `current_model_name` → rebuild.
/// - `last_indexed_commit` differs from current `git HEAD` → rebuild.
/// - `force_full == true` → rebuild unconditionally.
///
/// If `git::head_commit` fails (e.g. empty repo with no HEAD), the
/// commit field is treated as "always rebuild" and the resulting index
/// stores `last_indexed_commit = None`.
async fn load_or_rebuild_index(
    project_root: &Path,
    embedder: &Embedder,
    model_name: &str,
    force_full: bool,
) -> Result<IndexData> {
    let path = index_path(project_root)?;
    let head = git::head_commit(project_root).ok();

    if !force_full
        && let Some(existing) = store::load_index(&path)?
        && existing.embedding_model.as_deref() == Some(model_name)
        && let Some(head_hash) = head.as_deref()
        && existing.last_indexed_commit.as_deref() == Some(head_hash)
    {
        return Ok(existing);
    }

    let sources = config::resolve_sources(project_root)?;
    let mut data = run_index_for_sources(project_root, &sources, embedder).await?;
    data.last_indexed_commit = head;
    data.embedding_model = Some(model_name.to_string());
    store::save_index(&path, &data)?;
    Ok(data)
}

/// Core indexing pipeline.
///
/// `sources` is the caller-resolved list (so tests can inject one).
/// Returns an `IndexData` whose `chunks` map is populated; the caller
/// (`load_or_rebuild_index`) stamps `last_indexed_commit` and
/// `embedding_model` afterwards.
async fn run_index_for_sources(
    project_root: &Path,
    sources: &[SourceEntry],
    embedder: &Embedder,
) -> Result<IndexData> {
    let _root_span = info_span!("rebuild_index", project = %project_root.display()).entered();
    let index_start = std::time::Instant::now();

    if sources.is_empty() {
        warn!("no sources registered; rebuild produced empty index");
        return Ok(IndexData::new());
    }

    // Partition: drop dep sources with a structured warning, keep project sources.
    let mut project_sources: Vec<&SourceEntry> = Vec::new();
    for entry in sources {
        if entry.dep {
            warn!(
                root = %entry.root.display(),
                "dep sources are ignored in Phase 1 of ticket 260409; Phase 3 will implement \
                 dependency indexing"
            );
        } else {
            project_sources.push(entry);
        }
    }

    // ---- File discovery ---------------------------------------------------
    // Build (relative_path, language) pairs deduplicated across overlapping
    // source roots. The first source registered wins on a language conflict.
    //
    // Per-source strategy:
    // - External source (abs_root has its own .git, or lies outside project_root):
    //   use walkdir to enumerate files — git ls-files of the project repo cannot
    //   see files tracked by a foreign git repo.
    // - Project source: use git ls-files (existing behavior, single lazy call).
    let files: Vec<(PathBuf, Language)> = {
        let _span = info_span!("file_discovery").entered();

        let mut seen: HashMap<PathBuf, Language> = HashMap::new();
        let mut order: Vec<PathBuf> = Vec::new();

        // Lazily populated on first project-source entry.
        let mut git_paths: Option<Vec<PathBuf>> = None;

        for entry in &project_sources {
            let abs_root = project_root.join(&entry.root);
            let ext_set: HashSet<String> = entry
                .extensions
                .as_deref()
                .map(|exts| exts.iter().map(|e| e.to_ascii_lowercase()).collect())
                .unwrap_or_else(|| {
                    default_extensions(entry.language)
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect()
                });

            // Determine whether this source root is external to the project's
            // git repo. Two conditions indicate an external repo:
            // (a) the root has its own .git directory (a cloned sub-repo), but
            //     only when abs_root is not project_root itself — the project's
            //     own .git must not trigger the external branch, or
            // (b) it lies outside project_root entirely.
            let is_external = (abs_root != project_root && abs_root.join(".git").exists())
                || abs_root.strip_prefix(project_root).is_err();

            let candidate_paths: Vec<PathBuf> = if is_external {
                // Walk the external root directly.
                // Stored path is always relative to project_root: form it by
                // stripping abs_root from the file path, then prepending the
                // entry.root portion (which is itself project_root-relative for
                // inside-project sub-roots, or an absolute path for
                // fully-external roots — in the absolute case the stored key is
                // also absolute, and `project_root.join(absolute)` on POSIX
                // resolves to the absolute path, so file reads remain correct).
                let root_rel: &Path = abs_root
                    .strip_prefix(project_root)
                    .unwrap_or(abs_root.as_path());
                walkdir::WalkDir::new(&abs_root)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .filter_map(|e| {
                        let abs_path = e.into_path();
                        let within_root = abs_path.strip_prefix(&abs_root).ok()?;
                        let rel = root_rel.join(within_root);
                        let ext = lowercase_extension(&rel)?;
                        if ext_set.contains(&ext) {
                            Some(rel)
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                // Project source: use git ls-files (lazily fetched once).
                let all_paths = git_paths.get_or_insert_with(|| {
                    git::changed_files(project_root, None)
                        .map(|c| c.added.iter().map(PathBuf::from).collect())
                        .unwrap_or_default()
                });
                let scoped_root = entry_relative_root(&entry.root);
                all_paths
                    .iter()
                    .filter(|rel| path_under_root(rel, &scoped_root))
                    .filter(|rel| {
                        lowercase_extension(rel)
                            .map(|e| ext_set.contains(&e))
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            };

            for rel in candidate_paths {
                match seen.get(&rel) {
                    None => {
                        seen.insert(rel.clone(), entry.language);
                        order.push(rel);
                    }
                    Some(existing) if *existing == entry.language => {
                        // silent unify — same-language duplicate
                    }
                    Some(existing) => {
                        warn!(
                            path = %rel.display(),
                            kept = ?existing,
                            dropped = ?entry.language,
                            "file matched multiple sources with conflicting languages; \
                             keeping the language registered first"
                        );
                    }
                }
            }
        }

        order
            .into_iter()
            .map(|p| {
                let lang = seen[&p];
                (p, lang)
            })
            .collect()
    };

    // ---- Progress: discovery summary --------------------------------------
    {
        use std::io::Write;
        eprintln!(
            "[discovery]  {} files across {} sources",
            files.len(),
            project_sources.len()
        );
        let _ = std::io::stderr().flush();
    }

    // ---- Chunking ---------------------------------------------------------
    let total_files = files.len();
    let is_terminal = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut per_file: HashMap<PathBuf, Vec<Chunk>> = HashMap::new();
    {
        let _span = info_span!("chunking", files = total_files).entered();
        for (file_idx, (relative, language)) in files.iter().enumerate() {
            {
                use std::io::Write;
                let label = format!(
                    "[chunking]   file {}/{}  {}",
                    file_idx + 1,
                    total_files,
                    relative.display()
                );
                if is_terminal {
                    eprint!("\r{label:<80}");
                } else {
                    eprintln!("{label}");
                }
                let _ = std::io::stderr().flush();
            }
            let abs = project_root.join(relative);
            let source = match std::fs::read_to_string(&abs) {
                Ok(s) => s,
                Err(e) => {
                    warn!(path = %abs.display(), error = %e, "failed to read source file; skipping");
                    continue;
                }
            };
            let chunker = chunker_for(language);
            match chunker.chunk(relative, &source) {
                Ok(chunks) => {
                    per_file.insert(relative.clone(), chunks);
                }
                Err(e) => {
                    warn!(
                        path = %relative.display(),
                        error = %e,
                        "chunker failed; skipping file"
                    );
                }
            }
        }
    }

    // Terminate the chunking \r line on terminals before the embedding phase.
    if is_terminal && total_files > 0 {
        use std::io::Write;
        eprintln!();
        let _ = std::io::stderr().flush();
    }

    // ---- Embedding --------------------------------------------------------
    let total_chunks: usize = per_file.values().map(|v| v.len()).sum();
    {
        let _span = info_span!("embedding", chunks = total_chunks).entered();

        // Collect mutable refs in stable iteration order so embeddings can
        // be assigned by position after each batch.
        let mut chunk_refs: Vec<&mut Chunk> =
            per_file.values_mut().flat_map(|v| v.iter_mut()).collect();

        let total_batches = chunk_refs.len().div_ceil(EMBED_BATCH_SIZE);

        if !chunk_refs.is_empty() {
            use std::io::Write;
            let embed_start = std::time::Instant::now();
            let mut start = 0usize;
            let mut batch_idx = 0usize;
            while start < chunk_refs.len() {
                let end = (start + EMBED_BATCH_SIZE).min(chunk_refs.len());
                let texts: Vec<&str> = chunk_refs[start..end]
                    .iter()
                    .map(|c| c.embedding_text.as_str())
                    .collect();
                let embeddings = embedder.embed_batch(&texts)?;
                for (chunk, emb) in chunk_refs[start..end].iter_mut().zip(embeddings) {
                    chunk.embedding = Some(emb);
                }
                batch_idx += 1;
                let elapsed = embed_start.elapsed().as_secs_f64();
                let chunks_done = end;
                let rate = if elapsed > 0.0 {
                    chunks_done as f64 / elapsed
                } else {
                    0.0
                };
                let remaining = total_chunks.saturating_sub(chunks_done);
                let eta_secs = if rate > 0.0 {
                    (remaining as f64 / rate) as u64
                } else {
                    0
                };
                let eta_str = if eta_secs >= 60 {
                    format!("{}m{:02}s", eta_secs / 60, eta_secs % 60)
                } else {
                    format!("{eta_secs}s")
                };
                let label = format!(
                    "[embedding]  batch {batch_idx}/{total_batches}  ({:.0} chunks/s, ETA {eta_str})",
                    rate
                );
                if is_terminal {
                    eprint!("\r{label:<80}");
                } else {
                    eprintln!("{label}");
                }
                let _ = std::io::stderr().flush();
                start = end;
            }
            if is_terminal {
                eprintln!();
            }
        }

        let total_elapsed = index_start.elapsed();
        let total_secs = total_elapsed.as_secs();
        let total_str = if total_secs >= 60 {
            format!("{}m{:02}s", total_secs / 60, total_secs % 60)
        } else {
            format!("{total_secs}s")
        };
        eprintln!("done. {total_chunks} chunks, {total_files} files.  total {total_str}");
    }

    // ---- Index build ------------------------------------------------------
    let mut data = {
        let _span = info_span!("index_build").entered();
        let mut data = IndexData::new();
        data.chunks = per_file;
        data
    };
    // `last_indexed_commit` and `embedding_model` are stamped by the
    // caller — `run_index_for_sources` is intentionally agnostic about
    // which commit/model the rebuild ran against.
    data.last_indexed_commit = None;
    data.embedding_model = None;
    Ok(data)
}

// ---------------------------------------------------------------------------
// InProcessService
// ---------------------------------------------------------------------------

/// In-process service implementation. Executes all operations directly
/// in the CLI process without a daemon.
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

impl DebriefService for InProcessService {
    async fn index(
        &self,
        project_root: &Path,
        _path: &Path,
        include_deps: bool,
    ) -> Result<IndexResult> {
        if !include_deps {
            debug!("`--no-deps` is a no-op in Phase 1 of ticket 260409");
        } else {
            debug!("`include_deps = true` is a no-op in Phase 1 of ticket 260409");
        }
        let (embedder, model_name) = build_embedder(project_root).await?;
        let data = load_or_rebuild_index(project_root, &embedder, &model_name, true).await?;
        let files_indexed = data.chunks.len();
        let chunks_created = data.chunks.values().map(|v| v.len()).sum();
        Ok(IndexResult {
            files_indexed,
            chunks_created,
        })
    }

    async fn search(
        &self,
        project_root: &Path,
        query: &str,
        top_k: usize,
        include_deps: bool,
    ) -> Result<Vec<SearchResult>> {
        if include_deps {
            debug!("`include_deps = true` is a no-op in Phase 1 of ticket 260409");
        }
        let (embedder, model_name) = build_embedder(project_root).await?;
        let data = load_or_rebuild_index(project_root, &embedder, &model_name, false).await?;

        let flat: Vec<(PathBuf, Chunk)> = data
            .chunks
            .into_iter()
            .flat_map(|(path, chunks)| chunks.into_iter().map(move |c| (path.clone(), c)))
            .collect();

        let search_index = SearchIndex::build(flat)?;
        search_index.search(query, &embedder, top_k)
    }

    async fn overview(&self, project_root: &Path, file: &Path) -> Result<String> {
        let (embedder, model_name) = build_embedder(project_root).await?;
        let data = load_or_rebuild_index(project_root, &embedder, &model_name, false).await?;

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

        let chunks = data
            .chunks
            .get(&file_key)
            .ok_or_else(|| anyhow::anyhow!("no index entries for {}", file.display()))?;

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

    async fn dep_overview(&self, _project_root: &Path, _crate_name: &str) -> Result<String> {
        anyhow::bail!("dependency overview not yet available (Phase 3 of ticket 260409)")
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
        let target = if global {
            paths
                .global
                .context("could not determine global config path (no home directory?)")?
        } else {
            paths
                .project
                .context("not inside a git repository; cannot write project config")?
        };

        let mut config = config::load_layer_single(&target)?.unwrap_or_default();
        config.embedding_model = Some(model.to_string());
        save_config(&target, &config)
    }

    async fn add_source(
        &self,
        project_root: &Path,
        language: Language,
        root: &Path,
        dep: bool,
    ) -> Result<()> {
        let entry = SourceEntry {
            language,
            root: root.to_path_buf(),
            dep,
            extensions: None,
        };
        config::append_source(project_root, entry)
    }

    async fn list_sources(&self, project_root: &Path) -> Result<Vec<SourceEntry>> {
        config::resolve_sources(project_root)
    }

    async fn remove_source(&self, project_root: &Path, index: usize) -> Result<()> {
        config::remove_source_at(project_root, index)
    }
}

// ---------------------------------------------------------------------------
// DaemonClient
// ---------------------------------------------------------------------------

/// Client that communicates with a running daemon process via IPC.
///
/// Phase 1: the five legacy methods dispatch via the existing IPC
/// variants. Source registration (`add_source`/`list_sources`/
/// `remove_source`) does not have an IPC variant yet — those methods
/// `bail!`, which causes the surrounding `Service` wrapper to fall back
/// to `InProcessService`. This is the path that owns the project config
/// file anyway, so the fallback is semantically correct.
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
    async fn index(
        &self,
        _project_root: &Path,
        path: &Path,
        include_deps: bool,
    ) -> Result<IndexResult> {
        use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
        match self.send(DaemonRequest::Index {
            path: path.to_path_buf(),
            include_deps,
        })? {
            DaemonResponse::IndexResult(r) => Ok(r),
            DaemonResponse::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected response from daemon: {other:?}"),
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
            other => anyhow::bail!("unexpected response from daemon: {other:?}"),
        }
    }

    async fn overview(&self, _project_root: &Path, file: &Path) -> Result<String> {
        use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
        match self.send(DaemonRequest::Overview {
            file: file.to_path_buf(),
        })? {
            DaemonResponse::Overview { content } => Ok(content),
            DaemonResponse::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected response from daemon: {other:?}"),
        }
    }

    async fn dep_overview(&self, _project_root: &Path, crate_name: &str) -> Result<String> {
        use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
        match self.send(DaemonRequest::DepOverview {
            crate_name: crate_name.to_string(),
        })? {
            DaemonResponse::Overview { content } => Ok(content),
            DaemonResponse::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected response from daemon: {other:?}"),
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
            other => anyhow::bail!("unexpected response from daemon: {other:?}"),
        }
    }

    async fn add_source(
        &self,
        _project_root: &Path,
        _language: Language,
        _root: &Path,
        _dep: bool,
    ) -> Result<()> {
        // No IPC variant for source registration yet — fall back to in-process.
        anyhow::bail!("daemon dispatch for add_source not implemented in Phase 1")
    }

    async fn list_sources(&self, _project_root: &Path) -> Result<Vec<SourceEntry>> {
        anyhow::bail!("daemon dispatch for list_sources not implemented in Phase 1")
    }

    async fn remove_source(&self, _project_root: &Path, _index: usize) -> Result<()> {
        anyhow::bail!("daemon dispatch for remove_source not implemented in Phase 1")
    }
}

// ---------------------------------------------------------------------------
// Service — daemon-first dispatch with in-process fallback
// ---------------------------------------------------------------------------

/// Unified service dispatch: tries daemon first, falls back to in-process
/// on any IPC transport failure.
pub struct Service {
    daemon: Option<DaemonClient>,
    in_process: InProcessService,
}

impl Service {
    /// Create a service with both daemon and in-process backends.
    /// If daemon is `None`, all requests go directly to in-process.
    pub fn new(daemon: Option<DaemonClient>) -> Self {
        Self {
            daemon,
            in_process: InProcessService::new(),
        }
    }
}

impl DebriefService for Service {
    async fn index(
        &self,
        project_root: &Path,
        path: &Path,
        include_deps: bool,
    ) -> Result<IndexResult> {
        if let Some(d) = &self.daemon {
            match d.index(project_root, path, include_deps).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process
            .index(project_root, path, include_deps)
            .await
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

    async fn add_source(
        &self,
        project_root: &Path,
        language: Language,
        root: &Path,
        dep: bool,
    ) -> Result<()> {
        if let Some(d) = &self.daemon {
            match d.add_source(project_root, language, root, dep).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process
            .add_source(project_root, language, root, dep)
            .await
    }

    async fn list_sources(&self, project_root: &Path) -> Result<Vec<SourceEntry>> {
        if let Some(d) = &self.daemon {
            match d.list_sources(project_root).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process.list_sources(project_root).await
    }

    async fn remove_source(&self, project_root: &Path, index: usize) -> Result<()> {
        if let Some(d) = &self.daemon {
            match d.remove_source(project_root, index).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process.remove_source(project_root, index).await
    }
}
