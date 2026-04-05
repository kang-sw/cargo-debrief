use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::chunk::Chunk;

const INDEX_VERSION: u32 = 6;

#[derive(Serialize, Deserialize)]
pub struct IndexData {
    version: u32,
    pub last_indexed_commit: Option<String>,
    pub embedding_model: Option<String>,
    pub chunks: HashMap<PathBuf, Vec<Chunk>>,
}

impl IndexData {
    pub fn new() -> Self {
        Self {
            version: INDEX_VERSION,
            last_indexed_commit: None,
            embedding_model: None,
            chunks: HashMap::new(),
        }
    }
}

/// Save the index to disk. Creates parent directories if they don't exist.
pub fn save_index(path: &Path, data: &IndexData) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = bincode::serialize(data)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Load the index from disk.
/// Returns `Ok(None)` if the file doesn't exist or the version doesn't match `INDEX_VERSION`.
/// I/O or deserialization errors are returned as `Err`.
pub fn load_index(path: &Path) -> Result<Option<IndexData>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let data: IndexData = bincode::deserialize(&bytes)?;
    if data.version != INDEX_VERSION {
        return Ok(None);
    }
    Ok(Some(data))
}

// ---------------------------------------------------------------------------
// Dependency index
// ---------------------------------------------------------------------------

const DEPS_INDEX_VERSION: u32 = 3;

#[derive(Serialize, Deserialize)]
pub struct DepsIndexData {
    version: u32,
    pub cargo_lock_hash: Option<String>,
    pub chunks: Vec<Chunk>,
}

impl DepsIndexData {
    pub fn new() -> Self {
        Self {
            version: DEPS_INDEX_VERSION,
            cargo_lock_hash: None,
            chunks: vec![],
        }
    }
}

impl Default for DepsIndexData {
    fn default() -> Self {
        Self::new()
    }
}

/// Save the dependency index to disk. Creates parent directories if they don't exist.
pub fn save_deps_index(path: &Path, data: &DepsIndexData) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = bincode::serialize(data)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Load the dependency index from disk.
/// Returns `Ok(None)` if the file doesn't exist or the version doesn't match `DEPS_INDEX_VERSION`.
/// I/O or deserialization errors are returned as `Err`.
pub fn load_deps_index(path: &Path) -> Result<Option<DepsIndexData>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let data: DepsIndexData = bincode::deserialize(&bytes)?;
    if data.version != DEPS_INDEX_VERSION {
        return Ok(None);
    }
    Ok(Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{Chunk, ChunkKind, ChunkMetadata, ChunkOrigin, ChunkType, Visibility};

    fn sample_chunk(name: &str) -> Chunk {
        Chunk {
            display_text: format!("fn {name}() {{}}"),
            embedding_text: format!("function {name}"),
            metadata: ChunkMetadata {
                symbol_name: name.to_string(),
                kind: ChunkKind::Function,
                chunk_type: ChunkType::Function,
                parent: None,
                visibility: Visibility::Pub,
                file_path: "src/lib.rs".to_string(),
                line_range: (1, 3),
                signature: Some(format!("fn {name}()")),
            },
            embedding: Some(vec![0.1, 0.2, 0.3]),
            origin: ChunkOrigin::Project,
        }
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");

        let mut data = IndexData::new();
        data.last_indexed_commit = Some("abc123".to_string());
        data.embedding_model = Some("all-MiniLM-L6-v2".to_string());
        data.chunks.insert(
            PathBuf::from("src/lib.rs"),
            vec![sample_chunk("foo"), sample_chunk("bar")],
        );

        save_index(&path, &data).unwrap();
        let loaded = load_index(&path).unwrap().expect("expected Some");

        assert_eq!(loaded.version, INDEX_VERSION);
        assert_eq!(loaded.last_indexed_commit.as_deref(), Some("abc123"));
        assert_eq!(loaded.embedding_model.as_deref(), Some("all-MiniLM-L6-v2"));

        let chunks = loaded.chunks.get(&PathBuf::from("src/lib.rs")).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].metadata.symbol_name, "foo");
        assert_eq!(chunks[1].metadata.symbol_name, "bar");
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.bin");
        let result = load_index(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn version_mismatch_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");

        save_index(&path, &IndexData::new()).unwrap();

        // Overwrite the first 4 bytes (version u32, little-endian) with a wrong version
        let mut bytes = std::fs::read(&path).unwrap();
        let bad_version: u32 = 999;
        bytes[..4].copy_from_slice(&bad_version.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let result = load_index(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parent_directory_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("index.bin");
        assert!(!path.parent().unwrap().exists());

        save_index(&path, &IndexData::new()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_deps_index_staleness_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deps-index.bin");

        let mut data = DepsIndexData::new();
        data.cargo_lock_hash = Some("deadbeefcafe0000".to_string());
        data.chunks.push(sample_chunk("dep_fn"));

        save_deps_index(&path, &data).unwrap();
        let loaded = load_deps_index(&path).unwrap().expect("expected Some");

        assert_eq!(loaded.cargo_lock_hash.as_deref(), Some("deadbeefcafe0000"));
        assert_eq!(loaded.chunks.len(), 1);
        assert_eq!(loaded.chunks[0].metadata.symbol_name, "dep_fn");

        // Overwrite the first 4 bytes with a wrong version.
        let mut bytes = std::fs::read(&path).unwrap();
        let bad_version: u32 = 999;
        bytes[..4].copy_from_slice(&bad_version.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let result = load_deps_index(&path).unwrap();
        assert!(result.is_none(), "wrong version should return None");
    }
}
