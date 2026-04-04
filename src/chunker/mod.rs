use std::path::Path;

use anyhow::Result;

use crate::chunk::Chunk;

/// Language-extensible chunking interface.
pub trait Chunker {
    fn chunk(&self, file_path: &Path, source: &str) -> Result<Vec<Chunk>>;
}
