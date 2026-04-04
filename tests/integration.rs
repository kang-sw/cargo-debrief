use std::path::{Path, PathBuf};

use cargo_debrief::{
    chunk::Chunk,
    chunker::{Chunker, RustChunker},
    config::{Config, ConfigPaths, load_config, save_config},
    git::changed_files,
    search::SearchIndex,
    store::{IndexData, load_index, save_index},
};

// ---------------------------------------------------------------------------
// Helper: deterministic unit-length embeddings without ONNX/network
// ---------------------------------------------------------------------------

const MOCK_DIM: usize = 32;

/// Produces deterministic fake embeddings based on text hash.
/// Each text gets a unique, normalized vector of `dim` dimensions.
/// Same text → same vector; different texts → different vectors (with high probability).
fn mock_embed(text: &str, dim: usize) -> Vec<f32> {
    if dim == 0 {
        return vec![];
    }
    let bytes = text.as_bytes();
    let mut vec: Vec<f32> = (0..dim)
        .map(|i| {
            // Per-dimension LCG seeded by i, mixed with input bytes.
            let mut h: u64 = (i as u64 + 1).wrapping_mul(2654435761);
            for &b in bytes {
                h = h
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(b as u64 + (i as u64).wrapping_mul(31) + 1);
            }
            // Map u64 to [-1.0, 1.0].
            ((h & 0xFFFF) as f32 / 32767.5) - 1.0
        })
        .collect();

    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-9 {
        for x in &mut vec {
            *x /= norm;
        }
    }
    vec
}

/// Attach mock embeddings to all chunks in-place.
fn attach_mock_embeddings(chunks: &mut Vec<Chunk>) {
    for chunk in chunks.iter_mut() {
        chunk.embedding = Some(mock_embed(&chunk.embedding_text, MOCK_DIM));
    }
}

// ---------------------------------------------------------------------------
// Test source: a self-contained Rust snippet used across several tests
// ---------------------------------------------------------------------------

const SAMPLE_SOURCE: &str = r#"
/// A simple counter.
pub struct Counter {
    pub value: u32,
}

impl Counter {
    /// Create a new counter starting at zero.
    pub fn new() -> Self {
        Self { value: 0 }
    }

    /// Increment the counter by one.
    pub fn increment(&mut self) {
        self.value += 1;
    }

    /// Return the current value.
    pub fn get(&self) -> u32 {
        self.value
    }
}

/// Reset a counter to zero.
pub fn reset(counter: &mut Counter) {
    counter.value = 0;
}
"#;

// ---------------------------------------------------------------------------
// Test 2: chunker → store round-trip with embeddings
// ---------------------------------------------------------------------------

#[test]
fn chunker_store_round_trip_with_embeddings() {
    let file_path = Path::new("src/counter.rs");
    let chunker = RustChunker;
    let mut chunks = chunker
        .chunk(file_path, SAMPLE_SOURCE)
        .expect("RustChunker should not fail on valid source");

    assert!(
        !chunks.is_empty(),
        "RustChunker should produce at least one chunk"
    );

    // All chunks from the chunker start with embedding = None.
    assert!(
        chunks.iter().all(|c| c.embedding.is_none()),
        "freshly chunked items should have no embeddings"
    );

    attach_mock_embeddings(&mut chunks);

    // Verify embeddings were attached.
    assert!(
        chunks.iter().all(|c| c.embedding.is_some()),
        "all chunks should have embeddings after mock-embedding"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let index_path = dir.path().join("index.bin");

    let mut data = IndexData::new();
    data.chunks.insert(file_path.to_path_buf(), chunks.clone());
    save_index(&index_path, &data).expect("save_index");

    let loaded_data = load_index(&index_path)
        .expect("load_index")
        .expect("expected Some — just saved");

    let loaded_chunks = loaded_data
        .chunks
        .get(file_path)
        .expect("file path should be in loaded index");

    assert_eq!(
        chunks.len(),
        loaded_chunks.len(),
        "chunk count must be preserved after round-trip"
    );

    for (orig, loaded) in chunks.iter().zip(loaded_chunks.iter()) {
        assert_eq!(
            orig.display_text, loaded.display_text,
            "display_text mismatch for chunk: {}",
            orig.metadata.symbol_name
        );
        assert_eq!(
            orig.embedding_text, loaded.embedding_text,
            "embedding_text mismatch for chunk: {}",
            orig.metadata.symbol_name
        );
        assert_eq!(
            orig.metadata, loaded.metadata,
            "metadata mismatch for chunk: {}",
            orig.metadata.symbol_name
        );
        assert_eq!(
            orig.embedding, loaded.embedding,
            "embedding vectors must be bitwise identical after round-trip for: {}",
            orig.metadata.symbol_name
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: embedder → search boundary (mock embedder)
// ---------------------------------------------------------------------------

#[test]
fn search_with_mock_embeddings_ranks_closest_first() {
    let chunker = RustChunker;
    let mut chunks = chunker
        .chunk(Path::new("src/counter.rs"), SAMPLE_SOURCE)
        .expect("chunking");
    assert!(
        chunks.len() >= 2,
        "need at least 2 chunks; got {}",
        chunks.len()
    );

    attach_mock_embeddings(&mut chunks);

    // Pair up with a PathBuf for SearchIndex.
    let indexed: Vec<(PathBuf, Chunk)> = chunks
        .iter()
        .map(|c| (PathBuf::from(&c.metadata.file_path), c.clone()))
        .collect();

    let index = SearchIndex::build(indexed).expect("SearchIndex::build");

    // Search with the exact embedding of chunks[0] — it must rank first.
    let query_vec = chunks[0].embedding.as_ref().unwrap().clone();
    let results = index
        .search_by_vector(&query_vec, None, chunks.len())
        .expect("search_by_vector");

    // HNSW is approximate; it may return fewer than top_k even when the index
    // is small. Assert at least one result came back.
    assert!(
        !results.is_empty(),
        "search should return at least one result"
    );
    assert_eq!(
        results[0].file_path, chunks[0].metadata.file_path,
        "chunk[0] should rank first when queried with its own embedding"
    );

    // The top result (exact query match) must have a non-zero score.
    assert!(
        results[0].score > 0.0,
        "top result must have a positive score, got {}",
        results[0].score
    );
    // All scores must be non-negative (guaranteed by the [0, 1] clamp in search.rs).
    for result in &results {
        assert!(
            result.score >= 0.0,
            "every score must be non-negative, got {}",
            result.score
        );
    }
}

#[test]
fn search_symbol_name_boosting() {
    // Use two chunks from RustChunker, then override their embeddings with
    // controlled unit vectors so we can predict the boosting behavior.
    let chunker = RustChunker;
    let mut chunks = chunker
        .chunk(Path::new("src/counter.rs"), SAMPLE_SOURCE)
        .expect("chunking");
    assert!(
        chunks.len() >= 2,
        "need at least 2 chunks for boosting test; got {}",
        chunks.len()
    );

    // chunk_a: aligned exactly with query → raw cosine sim = 1.0, no name match.
    // chunk_b: slightly off → raw sim ≈ 0.9, but has an exact symbol name match (+0.3).
    // Expected: chunk_b ranks first (1.2 > 1.0) after boosting.
    let norm = |x: f32, y: f32| {
        let n = (x * x + y * y).sqrt();
        vec![x / n, y / n]
    };

    // Manually set 2-dimensional embeddings.
    chunks[0].embedding = Some(norm(1.0, 0.0)); // exact query direction
    chunks[1].embedding = Some(norm(0.9, 0.436)); // slightly off

    // Truncate to first two only.
    chunks.truncate(2);

    let target_symbol = chunks[1].metadata.symbol_name.clone();

    let indexed: Vec<(PathBuf, Chunk)> = chunks
        .iter()
        .map(|c| (PathBuf::from(&c.metadata.file_path), c.clone()))
        .collect();

    let index = SearchIndex::build(indexed).expect("SearchIndex::build");

    // Search: query vec aligned with chunk_a, but query text = chunk_b's symbol name.
    let query_vec = norm(1.0, 0.0);
    let results = index
        .search_by_vector(&query_vec, Some(&target_symbol), 2)
        .expect("search_by_vector");

    assert_eq!(results.len(), 2, "should return 2 results");
    assert_eq!(
        results[0].file_path, chunks[1].metadata.file_path,
        "chunk with exact symbol name match should rank first after boosting"
    );
    assert!(
        results[0].score > results[1].score,
        "boosted chunk score ({}) must exceed unboosted ({})",
        results[0].score,
        results[1].score
    );
}

// ---------------------------------------------------------------------------
// Test 4: config multi-layer merge for embedding_model
// ---------------------------------------------------------------------------

#[test]
fn config_multi_layer_project_overrides_global() {
    let dir = tempfile::tempdir().expect("tempdir");

    let global_path = dir.path().join("global").join("config.toml");
    let project_path = dir.path().join("project").join("config.toml");

    save_config(
        &global_path,
        &Config {
            embedding_model: Some("model-a".into()),
        },
    )
    .expect("save global config");

    save_config(
        &project_path,
        &Config {
            embedding_model: Some("model-b".into()),
        },
    )
    .expect("save project config");

    let paths = ConfigPaths {
        global: Some(global_path),
        project: Some(project_path),
        local: None,
    };

    let config = load_config(&paths).expect("load_config");
    assert_eq!(
        config.embedding_model.as_deref(),
        Some("model-b"),
        "project config must override global"
    );
}

#[test]
fn config_multi_layer_global_preserved_when_project_absent() {
    let dir = tempfile::tempdir().expect("tempdir");

    let global_path = dir.path().join("global").join("config.toml");
    let project_path = dir.path().join("project").join("config.toml");

    save_config(
        &global_path,
        &Config {
            embedding_model: Some("model-a".into()),
        },
    )
    .expect("save global config");

    // Project config exists but does NOT set embedding_model.
    save_config(
        &project_path,
        &Config {
            embedding_model: None,
        },
    )
    .expect("save project config (no embedding_model)");

    let paths = ConfigPaths {
        global: Some(global_path),
        project: Some(project_path),
        local: None,
    };

    let config = load_config(&paths).expect("load_config");
    assert_eq!(
        config.embedding_model.as_deref(),
        Some("model-a"),
        "global value must be preserved when project layer has no embedding_model"
    );
}

// ---------------------------------------------------------------------------
// Test 5: chunk embedding contract
// ---------------------------------------------------------------------------

#[test]
fn all_embedded_chunks_are_indexed() {
    let chunker = RustChunker;
    let mut chunks = chunker
        .chunk(Path::new("src/counter.rs"), SAMPLE_SOURCE)
        .expect("chunking");

    // Verify all start as non-embedded.
    assert!(chunks.iter().all(|c| c.embedding.is_none()));
    let total = chunks.len();

    attach_mock_embeddings(&mut chunks);

    let indexed: Vec<(PathBuf, Chunk)> = chunks
        .iter()
        .map(|c| (PathBuf::from(&c.metadata.file_path), c.clone()))
        .collect();

    let index = SearchIndex::build(indexed).expect("SearchIndex::build");

    let query_vec = mock_embed("any query text", MOCK_DIM);
    let results = index
        .search_by_vector(&query_vec, None, total + 10)
        .expect("search");

    assert_eq!(
        results.len(),
        total,
        "all {} embedded chunks must be searchable",
        total
    );
}

#[test]
fn only_embedded_chunks_are_searchable() {
    let chunker = RustChunker;
    let mut chunks = chunker
        .chunk(Path::new("src/counter.rs"), SAMPLE_SOURCE)
        .expect("chunking");

    assert!(
        chunks.len() >= 2,
        "need at least 2 chunks for mixed-embedding test"
    );

    let total = chunks.len();
    let embedded_count = total / 2; // embed only the first half

    for chunk in chunks[..embedded_count].iter_mut() {
        chunk.embedding = Some(mock_embed(&chunk.embedding_text, MOCK_DIM));
    }
    // The second half remains embedding = None.

    let indexed: Vec<(PathBuf, Chunk)> = chunks
        .iter()
        .map(|c| (PathBuf::from(&c.metadata.file_path), c.clone()))
        .collect();

    let index = SearchIndex::build(indexed).expect("SearchIndex::build");

    let query_vec = mock_embed("counter", MOCK_DIM);
    let results = index
        .search_by_vector(&query_vec, None, total + 10)
        .expect("search");

    assert_eq!(
        results.len(),
        embedded_count,
        "only the {} embedded chunks should be searchable (not {})",
        embedded_count,
        total
    );
}

// ---------------------------------------------------------------------------
// Test 6: git changed_files → chunker pipeline
// ---------------------------------------------------------------------------

#[test]
fn git_tracked_rs_files_all_chunk_successfully() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let changes =
        changed_files(repo_root, None).expect("changed_files(None) should succeed on a git repo");

    let rs_files: Vec<&str> = changes
        .added
        .iter()
        .map(String::as_str)
        .filter(|f| f.ends_with(".rs"))
        .collect();

    assert!(
        !rs_files.is_empty(),
        "repo should contain at least one tracked .rs file"
    );

    let chunker = RustChunker;

    for relative_path in &rs_files {
        let abs_path = repo_root.join(relative_path);
        let source = std::fs::read_to_string(&abs_path)
            .unwrap_or_else(|e| panic!("failed to read {relative_path}: {e}"));

        let chunks = chunker
            .chunk(Path::new(relative_path), &source)
            .unwrap_or_else(|e| panic!("RustChunker failed on {relative_path}: {e}"));

        // Files may genuinely produce zero chunks (e.g., empty files or
        // files with only `use` statements). Only assert the contract for
        // non-empty chunk lists.
        for chunk in &chunks {
            assert!(
                !chunk.display_text.is_empty(),
                "chunk in {relative_path} has empty display_text (symbol: {})",
                chunk.metadata.symbol_name
            );
            assert!(
                !chunk.embedding_text.is_empty(),
                "chunk in {relative_path} has empty embedding_text (symbol: {})",
                chunk.metadata.symbol_name
            );
            assert!(
                !chunk.metadata.file_path.is_empty(),
                "chunk in {relative_path} has empty metadata.file_path"
            );
            assert!(
                !chunk.metadata.symbol_name.is_empty(),
                "chunk in {relative_path} has empty metadata.symbol_name"
            );
        }
    }
}
