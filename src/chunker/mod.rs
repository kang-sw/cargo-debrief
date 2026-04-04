pub mod rust;

use std::path::Path;

use anyhow::Result;

use crate::chunk::Chunk;

pub use rust::RustChunker;

/// Language-extensible chunking interface.
pub trait Chunker {
    fn chunk(&self, file_path: &Path, source: &str) -> Result<Vec<Chunk>>;
}
