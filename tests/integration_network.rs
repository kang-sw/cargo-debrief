use std::path::{Path, PathBuf};

use cargo_debrief::{
    chunker::{Chunker, RustChunker},
    embedder::{Embedder, ModelRegistry},
    git::changed_files,
    search::SearchIndex,
};

// ---------------------------------------------------------------------------
// Skip helper
// ---------------------------------------------------------------------------

/// Returns true if network tests should be skipped.
/// Call at the top of each network test and return early if true:
///   `if skip_if_no_network() { return; }`
fn skip_if_no_network() -> bool {
    if std::env::var("CARGO_DEBRIEF_SKIP_NETWORK").is_ok() {
        eprintln!("Skipping network test (CARGO_DEBRIEF_SKIP_NETWORK set)");
        true
    } else {
        false
    }
}

fn model_cache_dir() -> PathBuf {
    dirs::data_dir()
        .expect("no data dir")
        .join("debrief")
        .join("models")
}

// ---------------------------------------------------------------------------
// Test source: a self-contained struct used in test 1
// ---------------------------------------------------------------------------

const CONNECTION_POOL_SOURCE: &str = r#"
/// Manages a pool of reusable connections.
pub struct ConnectionPool {
    max_size: usize,
    connections: Vec<Connection>,
}

pub struct Connection {
    id: u32,
}

impl ConnectionPool {
    /// Create a new pool with a given maximum size.
    pub fn new(max_size: usize) -> Self {
        Self {
            max_size,
            connections: Vec::new(),
        }
    }

    /// Acquire a connection from the pool, blocking until one is available.
    pub fn acquire(&mut self) -> Option<&Connection> {
        self.connections.first()
    }

    /// Return a connection to the pool for reuse.
    pub fn release(&mut self, id: u32) {
        self.connections.retain(|c| c.id != id);
    }
}
"#;

// ---------------------------------------------------------------------------
// Test 1: Real embedder + search end-to-end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn real_embedder_search_end_to_end() {
    if skip_if_no_network() {
        return;
    }

    let chunker = RustChunker;
    let mut chunks = chunker
        .chunk(Path::new("src/pool.rs"), CONNECTION_POOL_SOURCE)
        .expect("chunking should succeed");

    assert!(!chunks.is_empty(), "should produce at least one chunk");

    let cache_dir = model_cache_dir();
    let embedder = Embedder::load(ModelRegistry::DEFAULT_MODEL, &cache_dir)
        .await
        .expect("Embedder::load should succeed");

    let texts: Vec<&str> = chunks.iter().map(|c| c.embedding_text.as_str()).collect();
    let embeddings = embedder
        .embed_batch(&texts)
        .expect("embed_batch should succeed");

    assert_eq!(
        embeddings.len(),
        chunks.len(),
        "embed_batch must return one vector per input text"
    );

    // Verify dimension and normalization for every embedding.
    for (i, emb) in embeddings.iter().enumerate() {
        assert_eq!(
            emb.len(),
            768,
            "embedding[{i}] must have dimension 768, got {}",
            emb.len()
        );
        let norm: f32 = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "embedding[{i}] must be L2-normalized, got norm={norm}"
        );
    }

    // Attach embeddings to chunks.
    for (chunk, emb) in chunks.iter_mut().zip(embeddings) {
        chunk.embedding = Some(emb);
    }

    let indexed: Vec<(PathBuf, _)> = chunks
        .iter()
        .map(|c| (PathBuf::from(&c.metadata.file_path), c.clone()))
        .collect();

    let index = SearchIndex::build(indexed).expect("SearchIndex::build should succeed");

    // Search for "ConnectionPool" — the matching chunk must appear in top-3.
    let results = index
        .search("ConnectionPool", &embedder, 3)
        .expect("search should succeed");

    assert!(
        !results.is_empty(),
        "search must return at least one result"
    );

    let top_symbols: Vec<&str> = results.iter().map(|r| r.display_text.as_str()).collect();
    let found = results
        .iter()
        .any(|r| r.display_text.contains("ConnectionPool"));
    assert!(
        found,
        "ConnectionPool chunk must appear in top-3; got: {:?}",
        top_symbols
    );
}

// ---------------------------------------------------------------------------
// Test 2: Real chunker → embedder compatibility across the whole repo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn real_chunker_embedder_compatibility() {
    if skip_if_no_network() {
        return;
    }

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let changes =
        changed_files(repo_root, None).expect("changed_files should succeed on a git repo");

    let rs_files: Vec<PathBuf> = changes
        .added
        .iter()
        .filter(|f| f.ends_with(".rs"))
        .map(|f| repo_root.join(f))
        .collect();

    assert!(!rs_files.is_empty(), "repo should have tracked .rs files");

    let chunker = RustChunker;
    let mut all_texts: Vec<String> = Vec::new();

    for abs_path in &rs_files {
        let relative = abs_path
            .strip_prefix(repo_root)
            .unwrap_or(abs_path.as_path());
        let source = std::fs::read_to_string(abs_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", relative.display()));

        let chunks = chunker
            .chunk(relative, &source)
            .unwrap_or_else(|e| panic!("RustChunker failed on {}: {e}", relative.display()));

        for chunk in chunks {
            all_texts.push(chunk.embedding_text);
        }
    }

    assert!(
        !all_texts.is_empty(),
        "should have at least one chunk across all files"
    );

    let cache_dir = model_cache_dir();
    let embedder = Embedder::load(ModelRegistry::DEFAULT_MODEL, &cache_dir)
        .await
        .expect("Embedder::load should succeed");

    // Process in batches of 32 to avoid OOM.
    const BATCH_SIZE: usize = 32;
    let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(all_texts.len());

    for batch in all_texts.chunks(BATCH_SIZE) {
        let refs: Vec<&str> = batch.iter().map(String::as_str).collect();
        let batch_result = embedder
            .embed_batch(&refs)
            .expect("embed_batch should not fail on real Rust source");
        all_embeddings.extend(batch_result);
    }

    assert_eq!(
        all_embeddings.len(),
        all_texts.len(),
        "must get one embedding per chunk text"
    );

    for (i, emb) in all_embeddings.iter().enumerate() {
        assert_eq!(
            emb.len(),
            768,
            "embedding[{i}] must have dim 768, got {}",
            emb.len()
        );
        let norm: f32 = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "embedding[{i}] must be L2-normalized, got norm={norm}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: Semantic search quality smoke test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn semantic_search_quality_smoke_test() {
    if skip_if_no_network() {
        return;
    }

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let changes = changed_files(repo_root, None).expect("changed_files should succeed");

    let chunker = RustChunker;
    let cache_dir = model_cache_dir();
    let embedder = Embedder::load(ModelRegistry::DEFAULT_MODEL, &cache_dir)
        .await
        .expect("Embedder::load should succeed");

    // Collect all chunks from the repo.
    let mut all_pairs: Vec<(PathBuf, cargo_debrief::chunk::Chunk)> = Vec::new();
    let mut text_indices: Vec<usize> = Vec::new(); // maps chunk index → all_pairs index

    for relative in changes.added.iter().filter(|f| f.ends_with(".rs")) {
        let abs_path = repo_root.join(relative);
        let source = match std::fs::read_to_string(&abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let chunks = chunker
            .chunk(Path::new(relative), &source)
            .unwrap_or_default();

        for chunk in chunks {
            text_indices.push(all_pairs.len());
            all_pairs.push((PathBuf::from(relative), chunk));
        }
    }

    assert!(!all_pairs.is_empty(), "no chunks collected from repo");

    // Embed all chunks in batches of 32.
    let texts: Vec<&str> = all_pairs
        .iter()
        .map(|(_, c)| c.embedding_text.as_str())
        .collect();

    const BATCH_SIZE: usize = 32;
    let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for batch in texts.chunks(BATCH_SIZE) {
        let batch_result = embedder
            .embed_batch(batch)
            .expect("embed_batch should succeed");
        all_embeddings.extend(batch_result);
    }

    // Attach embeddings and build index.
    for ((_, chunk), emb) in all_pairs.iter_mut().zip(all_embeddings) {
        chunk.embedding = Some(emb);
    }

    let index = SearchIndex::build(all_pairs).expect("SearchIndex::build should succeed");

    // --- Query 1: config.rs should appear in top-5 for config-related query ---
    {
        let results = index
            .search("configuration file loading and merging", &embedder, 5)
            .expect("search should succeed");

        assert!(!results.is_empty(), "search should return results");
        let found = results.iter().any(|r| r.file_path.contains("config"));
        assert!(
            found,
            "a chunk from config.rs should appear in top-5 for config query; got: {:?}",
            results
                .iter()
                .map(|r| r.file_path.as_str())
                .collect::<Vec<_>>()
        );
    }

    // --- Query 2: chunker/rust.rs should appear in top-5 for tree-sitter query ---
    {
        let results = index
            .search("tree-sitter AST parsing", &embedder, 5)
            .expect("search should succeed");

        assert!(!results.is_empty(), "search should return results");
        let found = results
            .iter()
            .any(|r| r.file_path.contains("rust") || r.file_path.contains("chunker"));
        assert!(
            found,
            "a chunk from chunker/rust.rs should appear in top-5 for AST query; got: {:?}",
            results
                .iter()
                .map(|r| r.file_path.as_str())
                .collect::<Vec<_>>()
        );
    }

    // --- Query 3: "DebriefService" exact symbol match should rank #1 ---
    {
        let results = index
            .search("DebriefService", &embedder, 5)
            .expect("search should succeed");

        assert!(!results.is_empty(), "search should return results");
        assert!(
            results[0].file_path.contains("service"),
            "top result for 'DebriefService' should be from service.rs (symbol boost); got: {}",
            results[0].file_path
        );
    }
}

// ---------------------------------------------------------------------------
// Burn NomicBERT integration gate
// ---------------------------------------------------------------------------

/// Verifies that the burn NomicBERT path produces correct 768-dim L2-normalized embeddings.
/// Requires a network download of nomic-embed-text-v1.5 (~130MB, cached after first run).
#[tokio::test]
#[ignore]
async fn burn_nomic_bert_produces_normalized_768d_vectors() {
    if skip_if_no_network() {
        return;
    }

    let cache_dir = model_cache_dir();
    let embedder = Embedder::load("nomic-embed-text-v1.5", &cache_dir)
        .await
        .expect("failed to load burn embedder");

    let texts = [
        "fn main() { println!(\"hello world\"); }",
        "struct ConnectionPool { max_size: usize }",
    ];
    let embeddings = embedder.embed_batch(&texts).expect("embed_batch failed");

    assert_eq!(embeddings.len(), 2, "expected 2 output vectors");

    for (i, emb) in embeddings.iter().enumerate() {
        assert_eq!(
            emb.len(),
            768,
            "embedding {} should have 768 dims, got {}",
            i,
            emb.len()
        );
        let norm: f32 = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "embedding {} should be L2-normalized, got norm={norm}",
            i
        );
    }

    // The two texts are different — their cosine similarity should be < 0.99
    let dot: f32 = embeddings[0]
        .iter()
        .zip(embeddings[1].iter())
        .map(|(a, b)| a * b)
        .sum();
    assert!(
        dot < 0.99,
        "embeddings for different texts should not be near-identical (cosine={dot:.4})"
    );
}
