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

    /// Force a full re-index of a single source identified by `source_root`.
    ///
    /// `source_root` is matched against registered `SourceEntry.root` after
    /// canonicalizing both sides. Returns an error if no matching entry is found.
    fn rebuild_source(
        &self,
        project_root: &Path,
        source_root: &Path,
    ) -> impl Future<Output = Result<IndexResult>> + Send;
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

/// Convert a git-root-relative path to the chunk key used in `IndexData.chunks`.
///
/// Chunk keys follow the convention set by `run_index_for_sources`:
/// - For files within `project_root`, the key is project_root-relative.
/// - For external files (outside `project_root`), the key is absolute.
fn git_rel_to_chunk_key(git_root: &Path, git_rel: &Path, project_root: &Path) -> PathBuf {
    let abs_file = git_root.join(git_rel);
    abs_file
        .strip_prefix(project_root)
        .map(|p| p.to_path_buf())
        .unwrap_or(abs_file)
}

/// Chunk and embed a single source file, returning its chunks.
///
/// `file_key` is the path used as the `IndexData.chunks` key (may be relative
/// or absolute — the chunker receives it for metadata, the embedder text is
/// language-agnostic). Returns `Ok(vec![])` on read/chunk failure (logged as
/// warnings to match the batch pipeline).
fn chunk_file(abs_path: &Path, file_key: &Path, language: Language) -> Vec<Chunk> {
    let source = match std::fs::read_to_string(abs_path) {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %abs_path.display(), error = %e, "incremental: failed to read file");
            return vec![];
        }
    };
    let chunker = chunker_for(&language);
    match chunker.chunk(file_key, &source) {
        Ok(chunks) => chunks,
        Err(e) => {
            warn!(path = %abs_path.display(), error = %e, "incremental: chunker failed");
            vec![]
        }
    }
}

/// Embed a batch of chunks in-place using the given embedder.
fn embed_chunks(chunks: &mut Vec<Chunk>, embedder: &Embedder) -> Result<()> {
    let texts: Vec<&str> = chunks.iter().map(|c| c.embedding_text.as_str()).collect();
    if texts.is_empty() {
        return Ok(());
    }
    let embeddings = embedder.embed_batch(&texts)?;
    for (chunk, emb) in chunks.iter_mut().zip(embeddings) {
        chunk.embedding = Some(emb);
    }
    Ok(())
}

/// Stamp `git_states` for every git root reachable from `sources`.
///
/// Called after a full rebuild to record current HEAD + empty dirty snapshot
/// for all git roots. Also removes stale entries whose git root is no longer
/// referenced by any active source.
fn stamp_all_git_states(project_root: &Path, sources: &[SourceEntry], data: &mut IndexData) {
    let mut active_roots: HashSet<PathBuf> = HashSet::new();
    for entry in sources {
        if entry.dep {
            continue; // dep sources are not indexed; don't track their git roots
        }
        let abs_root = project_root.join(&entry.root);
        if let Some(git_root) = git::find_git_root(&abs_root) {
            let head = match git::current_head(&git_root) {
                Ok(h) => h,
                Err(e) => {
                    warn!(root = %git_root.display(), error = %e, "stamp_all_git_states: cannot get HEAD");
                    continue;
                }
            };
            let dirty = git::dirty_files(&git_root).unwrap_or_default();
            active_roots.insert(git_root.clone());
            data.git_states.insert(
                git_root,
                store::GitRepoState {
                    last_indexed_commit: head,
                    dirty_snapshot: store::DirtySnapshot { file_hashes: dirty },
                },
            );
        }
    }
    // Remove stale git_states entries no longer referenced by any source.
    data.git_states
        .retain(|root, _| active_roots.contains(root));
}

/// Apply incremental updates to `data` based on changes in each source's git repo.
///
/// Returns `true` when at least one source was patched (caller should save the index).
/// Returns `false` when every source is fully fresh (caller can return as-is).
///
/// For non-git sources: if no chunks exist under the source root, queues a full
/// walkdir scan of that source. Otherwise, skips (manual-only policy).
async fn apply_incremental_updates(
    project_root: &Path,
    sources: &[SourceEntry],
    data: &mut IndexData,
    embedder: &Embedder,
) -> Result<bool> {
    let mut any_change = false;

    // Track which git roots we've already processed (a root may be shared by
    // multiple source entries — process it only once).
    let mut processed_git_roots: HashSet<PathBuf> = HashSet::new();

    for entry in sources {
        if entry.dep {
            continue; // dep sources not indexed in Phase 1
        }

        let abs_root: PathBuf = if entry.root == Path::new(".") || entry.root.as_os_str().is_empty()
        {
            project_root.to_path_buf()
        } else {
            project_root.join(&entry.root)
        };

        let git_root = git::find_git_root(&abs_root);

        if git_root.is_none() {
            // Non-git source: full scan if no chunks exist under this root, else skip
            // (manual-only policy for stable external trees).
            // Assumption: registered non-git source roots are disjoint — if source B's
            // abs_root is a path prefix of an already-indexed source A's keys, B would
            // incorrectly see has_chunks=true. Callers should avoid overlapping non-git roots.
            let has_chunks = data.chunks.keys().any(|k| {
                let abs_k = project_root.join(k);
                abs_k.starts_with(&abs_root) || k.starts_with(&abs_root)
            });
            if !has_chunks {
                // Full scan this non-git source.
                let new_chunks = scan_source_full(project_root, entry, embedder).await?;
                for (key, chunks) in new_chunks {
                    data.chunks.insert(key, chunks);
                }
                any_change = true;
            }
            continue;
        }

        let git_root = git_root.unwrap();

        if processed_git_roots.contains(&git_root) {
            continue;
        }
        processed_git_roots.insert(git_root.clone());

        let current_head = match git::current_head(&git_root) {
            Ok(h) => h,
            Err(e) => {
                warn!(root = %git_root.display(), error = %e, "cannot get HEAD; skipping incremental for this root");
                continue;
            }
        };
        let current_dirty = git::dirty_files(&git_root).unwrap_or_default();

        let state = data.git_states.get(&git_root);

        // Compute commit-level changes since last indexed commit.
        let commit_changed = if let Some(s) = state {
            if s.last_indexed_commit != current_head {
                match git::changed_files(&git_root, Some(&s.last_indexed_commit)) {
                    Ok(fc) => fc,
                    Err(e) => {
                        warn!(root = %git_root.display(), error = %e,
                              "git diff failed; falling back to full rebuild for this root");
                        // Full rebuild for all sources under this git root.
                        // First, collect sources so we know their abs_roots for key removal.
                        let root_sources: Vec<&SourceEntry> = sources
                            .iter()
                            .filter(|e| {
                                !e.dep
                                    && git::find_git_root(&project_root.join(&e.root)).as_ref()
                                        == Some(&git_root)
                            })
                            .collect();

                        // Clear existing chunks for each source before re-scanning.
                        for src_entry in &root_sources {
                            let src_abs_root: PathBuf = if src_entry.root == Path::new(".")
                                || src_entry.root.as_os_str().is_empty()
                            {
                                project_root.to_path_buf()
                            } else {
                                project_root.join(&src_entry.root)
                            };
                            let src_root_rel = entry_relative_root(&src_entry.root);
                            let keys_to_remove: Vec<PathBuf> = data
                                .chunks
                                .keys()
                                .filter(|k| {
                                    let abs_k = if k.is_absolute() {
                                        k.to_path_buf()
                                    } else {
                                        project_root.join(k)
                                    };
                                    abs_k.starts_with(&src_abs_root)
                                        || (!src_root_rel.as_os_str().is_empty()
                                            && k.starts_with(&src_root_rel))
                                })
                                .cloned()
                                .collect();
                            for key in keys_to_remove {
                                data.chunks.remove(&key);
                            }
                        }

                        for src_entry in root_sources {
                            let new_chunks =
                                scan_source_full(project_root, src_entry, embedder).await?;
                            for (key, chunks) in new_chunks {
                                data.chunks.insert(key, chunks);
                            }
                        }
                        data.git_states.insert(
                            git_root.clone(),
                            store::GitRepoState {
                                last_indexed_commit: current_head,
                                dirty_snapshot: store::DirtySnapshot {
                                    file_hashes: current_dirty,
                                },
                            },
                        );
                        any_change = true;
                        continue;
                    }
                }
            } else {
                git::FileChanges {
                    added: vec![],
                    modified: vec![],
                    deleted: vec![],
                }
            }
        } else {
            // First time seeing this git root — full scan all sources under it.
            for src_entry in sources.iter().filter(|e| {
                !e.dep
                    && git::find_git_root(&project_root.join(&e.root)).as_ref() == Some(&git_root)
            }) {
                let new_chunks = scan_source_full(project_root, src_entry, embedder).await?;
                for (key, chunks) in new_chunks {
                    data.chunks.insert(key, chunks);
                }
            }
            data.git_states.insert(
                git_root.clone(),
                store::GitRepoState {
                    last_indexed_commit: current_head,
                    dirty_snapshot: store::DirtySnapshot {
                        file_hashes: current_dirty,
                    },
                },
            );
            any_change = true;
            continue;
        };

        // Determine which files changed in the dirty working tree.
        let empty_state = store::GitRepoState {
            last_indexed_commit: String::new(),
            dirty_snapshot: store::DirtySnapshot::default(),
        };
        let prev_dirty = state.unwrap_or(&empty_state);

        let mut dirty_changed: HashSet<PathBuf> = HashSet::new();
        for (path, hash) in &current_dirty {
            let prev_hash = prev_dirty.dirty_snapshot.file_hashes.get(path);
            if prev_hash != Some(hash) {
                dirty_changed.insert(path.clone());
            }
        }

        // Files that were dirty last time, are now clean, and are not in commit_changed
        // have been reverted — re-index them to HEAD version.
        // Keep paths in PathBuf domain to avoid silent UTF-8 lossy conversions.
        let commit_all_paths: HashSet<&Path> = commit_changed
            .added
            .iter()
            .chain(&commit_changed.modified)
            .chain(&commit_changed.deleted)
            .map(|s| Path::new(s.as_str()))
            .collect();

        let mut reverted: HashSet<PathBuf> = HashSet::new();
        for path in prev_dirty.dirty_snapshot.file_hashes.keys() {
            if !current_dirty.contains_key(path) && !commit_all_paths.contains(path.as_path()) {
                reverted.insert(path.clone());
            }
        }

        let files_to_reindex: HashSet<PathBuf> = commit_changed
            .added
            .iter()
            .chain(&commit_changed.modified)
            .map(PathBuf::from)
            .chain(dirty_changed.into_iter())
            .chain(reverted.into_iter())
            .collect();

        let files_to_remove: HashSet<PathBuf> =
            commit_changed.deleted.iter().map(PathBuf::from).collect();

        if files_to_reindex.is_empty() && files_to_remove.is_empty() {
            // Nothing changed for this git root; update git_states and continue.
            data.git_states.insert(
                git_root,
                store::GitRepoState {
                    last_indexed_commit: current_head,
                    dirty_snapshot: store::DirtySnapshot {
                        file_hashes: current_dirty,
                    },
                },
            );
            continue;
        }

        // Determine language for each affected file by checking source entries.
        // Build a map: abs_root → (entry_relative_root, language, ext_set) for all
        // sources under this git root.
        struct SourceInfo {
            abs_root: PathBuf,
            root_rel: PathBuf,
            language: Language,
            ext_set: HashSet<String>,
        }
        let source_infos: Vec<SourceInfo> = sources
            .iter()
            .filter(|e| {
                !e.dep
                    && git::find_git_root(&project_root.join(&e.root)).as_ref() == Some(&git_root)
            })
            .map(|e| {
                let ar: PathBuf = if e.root == Path::new(".") || e.root.as_os_str().is_empty() {
                    project_root.to_path_buf()
                } else {
                    project_root.join(&e.root)
                };
                let rr = entry_relative_root(&e.root);
                let ext_set: HashSet<String> = e
                    .extensions
                    .as_deref()
                    .map(|exts| exts.iter().map(|ex| ex.to_ascii_lowercase()).collect())
                    .unwrap_or_else(|| {
                        default_extensions(e.language)
                            .iter()
                            .map(|s| s.to_string())
                            .collect()
                    });
                SourceInfo {
                    abs_root: ar,
                    root_rel: rr,
                    language: e.language,
                    ext_set,
                }
            })
            .collect();

        // Remove chunks for deleted files.
        for rel_path in &files_to_remove {
            let chunk_key = git_rel_to_chunk_key(&git_root, rel_path, project_root);
            data.chunks.remove(&chunk_key);
        }

        // Re-chunk and re-embed files to reindex.
        for rel_path in &files_to_reindex {
            let abs_file = git_root.join(rel_path);
            // Find the source info that owns this file.
            let info = source_infos.iter().find(|si| {
                abs_file.starts_with(&si.abs_root)
                    && lowercase_extension(&abs_file)
                        .map(|e| si.ext_set.contains(&e))
                        .unwrap_or(false)
            });
            let info = match info {
                Some(i) => i,
                None => continue, // file not in any indexed source or wrong extension
            };

            // Compute chunk key: root_rel + file_within_abs_root
            let within_abs_root = abs_file
                .strip_prefix(&info.abs_root)
                .unwrap_or(rel_path.as_path());
            let chunk_key = info.root_rel.join(within_abs_root);

            let mut chunks = chunk_file(&abs_file, &chunk_key, info.language);
            if let Err(e) = embed_chunks(&mut chunks, embedder) {
                warn!(path = %abs_file.display(), error = %e, "incremental: embedding failed");
                continue;
            }
            if chunks.is_empty() {
                data.chunks.remove(&chunk_key);
            } else {
                data.chunks.insert(chunk_key, chunks);
            }
        }

        // Update git_states for this root.
        data.git_states.insert(
            git_root,
            store::GitRepoState {
                last_indexed_commit: current_head,
                dirty_snapshot: store::DirtySnapshot {
                    file_hashes: current_dirty,
                },
            },
        );
        any_change = true;
    }

    Ok(any_change)
}

/// Perform a full walkdir/ls-files scan of a single `SourceEntry`, returning
/// chunk key → chunks pairs ready for insertion into `IndexData.chunks`.
///
/// Used by incremental update (first-time source or git-diff fallback) and
/// Phase 3 per-source rebuild.
async fn scan_source_full(
    project_root: &Path,
    entry: &SourceEntry,
    embedder: &Embedder,
) -> Result<HashMap<PathBuf, Vec<Chunk>>> {
    let abs_root: PathBuf = if entry.root == Path::new(".") || entry.root.as_os_str().is_empty() {
        project_root.to_path_buf()
    } else {
        project_root.join(&entry.root)
    };
    let root_rel = entry_relative_root(&entry.root);

    let ext_set: HashSet<String> = entry
        .extensions
        .as_deref()
        .map(|exts| exts.iter().map(|e| e.to_ascii_lowercase()).collect())
        .unwrap_or_else(|| {
            default_extensions(entry.language)
                .iter()
                .map(|s| s.to_string())
                .collect()
        });

    let is_external = (abs_root != project_root && abs_root.join(".git").exists())
        || abs_root.strip_prefix(project_root).is_err();

    let candidate_keys: Vec<PathBuf> = if is_external {
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
        let all_paths = git::changed_files(project_root, None)
            .map(|c| c.added.iter().map(PathBuf::from).collect::<Vec<_>>())
            .unwrap_or_default();
        let scoped_root = entry_relative_root(&entry.root);
        all_paths
            .into_iter()
            .filter(|rel| path_under_root(rel, &scoped_root))
            .filter(|rel| {
                lowercase_extension(rel)
                    .map(|e| ext_set.contains(&e))
                    .unwrap_or(false)
            })
            .collect()
    };

    let mut result: HashMap<PathBuf, Vec<Chunk>> = HashMap::new();
    for key in candidate_keys {
        // project_root.join is a no-op for absolute keys (POSIX: absolute component wins).
        let abs_file = project_root.join(&key);
        let mut chunks = chunk_file(&abs_file, &key, entry.language);
        if chunks.is_empty() {
            continue;
        }
        if let Err(e) = embed_chunks(&mut chunks, embedder) {
            warn!(path = %abs_file.display(), error = %e, "scan_source_full: embedding failed");
            continue;
        }
        result.insert(key, chunks);
    }
    Ok(result)
}

/// Resolve the on-disk index, performing a full rebuild or incremental update.
///
/// Staleness rules (in priority order):
/// - `force_full == true` → full rebuild unconditionally.
/// - Missing file or version/backend mismatch → full rebuild (handled by `load_index`).
/// - `embedding_model` differs from `current_model_name` → full rebuild.
/// - Otherwise: apply per-git-root incremental staleness check (Phase 2).
async fn load_or_rebuild_index(
    project_root: &Path,
    embedder: &Embedder,
    model_name: &str,
    force_full: bool,
) -> Result<IndexData> {
    let path = index_path(project_root)?;

    if !force_full {
        if let Some(mut existing) = store::load_index(&path)? {
            if existing.embedding_model.as_deref() == Some(model_name) {
                let sources = config::resolve_sources(project_root)?;
                let changed =
                    apply_incremental_updates(project_root, &sources, &mut existing, embedder)
                        .await?;

                // Rebuild HNSW hint: callers rebuild from full data.chunks on each search,
                // so no separate rebuild step is needed here. Just persist if changed.
                if changed {
                    existing.embedding_model = Some(model_name.to_string());
                    store::save_index(&path, &existing)?;
                }
                return Ok(existing);
            }
        }
    }

    // Full rebuild.
    let sources = config::resolve_sources(project_root)?;
    let mut data = run_index_for_sources(project_root, &sources, embedder).await?;

    // Stamp git_states for all sources.
    stamp_all_git_states(project_root, &sources, &mut data);
    data.embedding_model = Some(model_name.to_string());
    store::save_index(&path, &data)?;
    Ok(data)
}

/// Core indexing pipeline.
///
/// `sources` is the caller-resolved list (so tests can inject one).
/// Returns an `IndexData` whose `chunks` map is populated; the caller
/// (`load_or_rebuild_index`) stamps `git_states` and
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
    // `git_states` and `embedding_model` are stamped by the
    // caller — `run_index_for_sources` is intentionally agnostic about
    // which commit/model the rebuild ran against.
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

    async fn rebuild_source(&self, project_root: &Path, source_root: &Path) -> Result<IndexResult> {
        // Canonicalize the requested path for comparison.
        let canon_request = source_root
            .canonicalize()
            .unwrap_or_else(|_| source_root.to_path_buf());

        let sources = config::resolve_sources(project_root)?;

        // Find the matching SourceEntry by canonicalizing both sides.
        let entry = sources
            .iter()
            .find(|e| {
                let abs = project_root.join(&e.root);
                let canon = abs.canonicalize().unwrap_or(abs);
                canon == canon_request
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no registered source matches {:?}; run `sources` to list registered sources",
                    source_root
                )
            })?
            .clone();

        let (embedder, model_name) = build_embedder(project_root).await?;
        let index_path = index_path(project_root)?;

        // Load existing index or start fresh.
        let mut data = store::load_index(&index_path)?
            .filter(|d| d.embedding_model.as_deref() == Some(&model_name))
            .unwrap_or_else(IndexData::new);

        // Clear all existing chunks under this source root.
        let abs_root: PathBuf = if entry.root == Path::new(".") || entry.root.as_os_str().is_empty()
        {
            project_root.to_path_buf()
        } else {
            project_root.join(&entry.root)
        };
        let root_rel = entry_relative_root(&entry.root);

        // Determine keys to remove: both project-relative and absolute-prefix matches.
        let keys_to_remove: Vec<PathBuf> = data
            .chunks
            .keys()
            .filter(|k| {
                // Absolute key: starts_with abs_root
                // Relative key: starts_with root_rel (non-empty) or abs_root relative to project_root
                let abs_k = if k.is_absolute() {
                    k.to_path_buf()
                } else {
                    project_root.join(k)
                };
                abs_k.starts_with(&abs_root)
                    || (!root_rel.as_os_str().is_empty() && k.starts_with(&root_rel))
            })
            .cloned()
            .collect();
        for key in &keys_to_remove {
            data.chunks.remove(key);
        }

        // Full re-scan of this source.
        let new_chunks = scan_source_full(project_root, &entry, &embedder).await?;
        let files_indexed = new_chunks.len();
        let chunks_created: usize = new_chunks.values().map(|v| v.len()).sum();
        for (key, chunks) in new_chunks {
            data.chunks.insert(key, chunks);
        }

        // Update git_states for this source's git root.
        if let Some(git_root) = git::find_git_root(&abs_root) {
            if let Ok(head) = git::current_head(&git_root) {
                let dirty = git::dirty_files(&git_root).unwrap_or_default();
                data.git_states.insert(
                    git_root,
                    store::GitRepoState {
                        last_indexed_commit: head,
                        dirty_snapshot: store::DirtySnapshot { file_hashes: dirty },
                    },
                );
            }
        }

        data.embedding_model = Some(model_name);
        store::save_index(&index_path, &data)?;

        Ok(IndexResult {
            files_indexed,
            chunks_created,
        })
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

    async fn rebuild_source(
        &self,
        _project_root: &Path,
        _source_root: &Path,
    ) -> Result<IndexResult> {
        anyhow::bail!(
            "daemon dispatch for rebuild_source not implemented; falling back to in-process"
        )
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

    async fn rebuild_source(&self, project_root: &Path, source_root: &Path) -> Result<IndexResult> {
        if let Some(d) = &self.daemon {
            match d.rebuild_source(project_root, source_root).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process
            .rebuild_source(project_root, source_root)
            .await
    }
}
