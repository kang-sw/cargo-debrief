use std::path::PathBuf;

use anyhow::Result;
use hnsw_rs::prelude::*;

use crate::chunk::Chunk;
use crate::embedder::Embedder;
use crate::service::SearchResult;

/// Score boost applied when query text exactly matches a chunk's symbol name (case-insensitive).
const EXACT_SYMBOL_MATCH_BOOST: f64 = 0.3;
/// Score boost applied when query text partially matches a chunk's symbol name (substring, case-insensitive).
const PARTIAL_SYMBOL_MATCH_BOOST: f64 = 0.1;

const HNSW_MAX_CONNECTIONS: usize = 16;
const HNSW_MAX_LAYERS: usize = 16;
const HNSW_EF_CONSTRUCTION: usize = 200;
const HNSW_EF_SEARCH: usize = 50;

/// In-memory vector search index built from embedded chunks.
///
/// Wraps an HNSW approximate-nearest-neighbour index. Chunks are stored
/// in a parallel `Vec` so integer IDs returned by HNSW map directly to
/// their corresponding chunk and file path.
pub struct SearchIndex {
    hnsw: Option<Hnsw<'static, f32, DistCosine>>,
    chunks: Vec<(PathBuf, Chunk)>,
}

impl SearchIndex {
    /// Build an index from chunks that have embeddings.
    /// Chunks without embeddings (`embedding == None`) are skipped.
    pub fn build(chunks: Vec<(PathBuf, Chunk)>) -> Result<Self> {
        let embedded: Vec<(PathBuf, Chunk)> = chunks
            .into_iter()
            .filter(|(_, c)| c.embedding.is_some())
            .collect();

        if embedded.is_empty() {
            return Ok(Self {
                hnsw: None,
                chunks: embedded,
            });
        }

        let max_elements = embedded.len();
        let hnsw = Hnsw::new(
            HNSW_MAX_CONNECTIONS,
            max_elements,
            HNSW_MAX_LAYERS,
            HNSW_EF_CONSTRUCTION,
            DistCosine,
        );

        for (id, (_, chunk)) in embedded.iter().enumerate() {
            let emb = chunk.embedding.as_ref().unwrap();
            hnsw.insert((emb.as_slice(), id));
        }

        Ok(Self {
            hnsw: Some(hnsw),
            chunks: embedded,
        })
    }

    /// Search the index using a pre-computed embedding vector.
    ///
    /// `query_text` is used only for metadata score boosting; pass `None` to skip boosting.
    /// Results are sorted by final score (descending) and capped at `top_k`.
    pub fn search_by_vector(
        &self,
        query_vec: &[f32],
        query_text: Option<&str>,
        top_k: usize,
    ) -> Result<Vec<SearchResult>> {
        if self.chunks.is_empty() || top_k == 0 {
            return Ok(vec![]);
        }

        let hnsw = self.hnsw.as_ref().unwrap();
        let neighbors = hnsw.search(query_vec, top_k, HNSW_EF_SEARCH);

        let mut results: Vec<SearchResult> = neighbors
            .iter()
            .map(|n| {
                let (_, chunk) = &self.chunks[n.d_id];
                let raw_similarity = (1.0_f64 - n.distance as f64).clamp(0.0_f64, 1.0_f64);
                let score =
                    apply_symbol_boost(raw_similarity, query_text, &chunk.metadata.symbol_name);

                SearchResult {
                    file_path: chunk.metadata.file_path.clone(),
                    line_range: chunk.metadata.line_range,
                    score,
                    display_text: chunk.display_text.clone(),
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(top_k);
        Ok(results)
    }

    /// Search the index. Embeds the query using the provided embedder,
    /// runs ANN search, applies metadata score boosting, and returns
    /// top-k results sorted by final score (descending).
    pub fn search(
        &self,
        query: &str,
        embedder: &Embedder,
        top_k: usize,
    ) -> Result<Vec<SearchResult>> {
        let query_vec = embedder.embed(query)?;
        self.search_by_vector(&query_vec, Some(query), top_k)
    }
}

/// Apply symbol-name–based score boosting.
///
/// Exact match (case-insensitive) adds `EXACT_SYMBOL_MATCH_BOOST`.
/// Partial match (either is a substring of the other) adds `PARTIAL_SYMBOL_MATCH_BOOST`.
fn apply_symbol_boost(raw_score: f64, query_text: Option<&str>, symbol_name: &str) -> f64 {
    let Some(query) = query_text else {
        return raw_score;
    };

    let query_lower = query.to_lowercase();
    let symbol_lower = symbol_name.to_lowercase();

    if query_lower == symbol_lower {
        raw_score + EXACT_SYMBOL_MATCH_BOOST
    } else if query_lower.contains(&symbol_lower) || symbol_lower.contains(&query_lower) {
        raw_score + PARTIAL_SYMBOL_MATCH_BOOST
    } else {
        raw_score
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{ChunkKind, ChunkMetadata, ChunkType, Visibility};

    fn make_chunk(symbol_name: &str, file_path: &str, embedding: Option<Vec<f32>>) -> Chunk {
        Chunk {
            display_text: format!("fn {}() {{}}", symbol_name),
            embedding_text: symbol_name.to_string(),
            metadata: ChunkMetadata {
                symbol_name: symbol_name.to_string(),
                kind: ChunkKind::Function,
                chunk_type: ChunkType::Function,
                parent: None,
                visibility: Visibility::Pub,
                file_path: file_path.to_string(),
                line_range: (1, 5),
                signature: None,
            },
            embedding,
        }
    }

    fn unit_vec(x: f32, y: f32, z: f32) -> Vec<f32> {
        let norm = (x * x + y * y + z * z).sqrt();
        vec![x / norm, y / norm, z / norm]
    }

    #[test]
    fn build_empty_index() {
        let index = SearchIndex::build(vec![]).unwrap();
        let results = index.search_by_vector(&[1.0, 0.0, 0.0], None, 5).unwrap();
        assert!(results.is_empty(), "empty index should return no results");
    }

    #[test]
    fn build_and_search() {
        // chunk_a is aligned with query; chunk_b is orthogonal.
        let chunk_a = make_chunk("func_a", "src/a.rs", Some(unit_vec(1.0, 0.0, 0.0)));
        let chunk_b = make_chunk("func_b", "src/b.rs", Some(unit_vec(0.0, 1.0, 0.0)));

        let chunks = vec![
            (PathBuf::from("src/a.rs"), chunk_a),
            (PathBuf::from("src/b.rs"), chunk_b),
        ];

        let index = SearchIndex::build(chunks).unwrap();
        let query = unit_vec(1.0, 0.0, 0.0);
        let results = index.search_by_vector(&query, None, 2).unwrap();

        assert_eq!(results.len(), 2);
        assert!(
            results[0].score > results[1].score,
            "first result should have higher score than second"
        );
        assert_eq!(
            results[0].file_path, "src/a.rs",
            "chunk_a should be ranked first"
        );
    }

    #[test]
    fn metadata_boost_exact_match() {
        // chunk_a has higher raw cosine similarity but no symbol name match.
        // chunk_b has lower raw similarity but exact symbol name match.
        // After boosting, chunk_b should rank first.
        let query_vec = unit_vec(1.0, 0.0, 0.0);
        let query_text = "target_func";

        // chunk_a: exact same direction as query → sim = 1.0, no name match
        let chunk_a = make_chunk("other_func", "src/a.rs", Some(unit_vec(1.0, 0.0, 0.0)));
        // chunk_b: slightly off direction → sim ≈ 0.9, exact name match (+0.3 boost → 1.2)
        let chunk_b = make_chunk("target_func", "src/b.rs", Some(unit_vec(0.9, 0.436, 0.0)));

        let chunks = vec![
            (PathBuf::from("src/a.rs"), chunk_a),
            (PathBuf::from("src/b.rs"), chunk_b),
        ];

        let index = SearchIndex::build(chunks).unwrap();
        let results = index
            .search_by_vector(&query_vec, Some(query_text), 2)
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].file_path, "src/b.rs",
            "chunk with exact symbol name match should rank first after boosting"
        );
        assert!(
            results[0].score > results[1].score,
            "boosted chunk should have strictly higher score"
        );
    }

    #[test]
    fn chunks_without_embeddings_skipped() {
        let chunk_with_embedding = make_chunk("func_a", "src/a.rs", Some(unit_vec(1.0, 0.0, 0.0)));
        let chunk_without_embedding = make_chunk("func_b", "src/b.rs", None);

        let chunks = vec![
            (PathBuf::from("src/a.rs"), chunk_with_embedding),
            (PathBuf::from("src/b.rs"), chunk_without_embedding),
        ];

        let index = SearchIndex::build(chunks).unwrap();
        assert_eq!(
            index.chunks.len(),
            1,
            "only the chunk with embedding should be indexed"
        );

        let results = index
            .search_by_vector(&unit_vec(1.0, 0.0, 0.0), None, 5)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_path, "src/a.rs");
    }
}
