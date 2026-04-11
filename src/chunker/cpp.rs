//! C++ chunker stub.
//!
//! Skeleton only — populated in Phase 2 of the multi-language sources
//! epic. Uses `tree-sitter-cpp` for AST-aware chunking of `.cpp/.h/.hpp/...`
//! sources.

use std::path::Path;

use anyhow::Result;

use crate::chunk::Chunk;

use super::Chunker;

/// Tree-sitter-cpp-backed chunker. Stub until Phase 2.
pub struct CppChunker;

impl Chunker for CppChunker {
    fn chunk(&self, _file_path: &Path, _source: &str) -> Result<Vec<Chunk>> {
        todo!("CppChunker: implement tree-sitter-cpp two-pass chunking in Phase 2")
    }
}
