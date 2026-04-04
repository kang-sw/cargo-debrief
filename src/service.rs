use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{
    chunk::{Chunk, ChunkType, Visibility},
    chunker::{Chunker, RustChunker},
    config::{config_paths, load_config, save_config},
    embedder::{Embedder, ModelRegistry},
    git,
    search::SearchIndex,
    store::{self, IndexData},
};

/// Result of an indexing operation.
#[derive(Debug)]
pub struct IndexResult {
    pub files_indexed: usize,
    pub chunks_created: usize,
}

/// A single search result with relevance score.
#[derive(Debug)]
pub struct SearchResult {
    pub file_path: String,
    pub line_range: (usize, usize),
    pub score: f64,
    pub display_text: String,
    pub module_path: String,
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
    ) -> impl Future<Output = Result<Vec<SearchResult>>> + Send;

    fn overview(
        &self,
        project_root: &Path,
        file: &Path,
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
async fn ensure_index_fresh(project_root: &Path) -> Result<IndexData> {
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

    let (data, _) = run_index(project_root, prior_commit.as_deref(), base_index).await?;
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
) -> Result<(IndexData, IndexResult)> {
    let paths = config_paths(project_root);
    let config = load_config(&paths)?;
    let model_name = config
        .embedding_model
        .as_deref()
        .unwrap_or(ModelRegistry::DEFAULT_MODEL);

    let embedder = make_embedder(model_name).await?;

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
// DebriefService implementation
// ---------------------------------------------------------------------------

impl DebriefService for InProcessService {
    async fn index(&self, project_root: &Path, _path: &Path) -> Result<IndexResult> {
        // Full reindex: no prior commit, no existing index.
        let (_data, result) = run_index(project_root, None, None).await?;
        Ok(result)
    }

    async fn search(
        &self,
        project_root: &Path,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchResult>> {
        let index_data = ensure_index_fresh(project_root).await?;
        let config = load_config(&config_paths(project_root))?;
        let model_name = config
            .embedding_model
            .as_deref()
            .unwrap_or(ModelRegistry::DEFAULT_MODEL);
        let embedder = make_embedder(model_name).await?;

        let flat_chunks: Vec<(PathBuf, Chunk)> = index_data
            .chunks
            .into_iter()
            .flat_map(|(path, chunks)| chunks.into_iter().map(move |c| (path.clone(), c)))
            .collect();

        let search_index = SearchIndex::build(flat_chunks)?;
        search_index.search(query, &embedder, top_k)
    }

    async fn overview(&self, project_root: &Path, file: &Path) -> Result<String> {
        let index_data = ensure_index_fresh(project_root).await?;

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
}
