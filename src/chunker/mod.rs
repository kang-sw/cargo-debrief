pub mod cpp;
pub mod rust;

use std::path::Path;

use anyhow::Result;

use crate::chunk::Chunk;
use crate::config::Language;

pub use cpp::CppChunker;
pub use rust::RustChunker;

/// Language-extensible chunking interface.
pub trait Chunker {
    fn chunk(&self, file_path: &Path, source: &str) -> Result<Vec<Chunk>>;
}

/// Dispatch: construct a chunker for the given language.
///
/// This is the sole routing point between `Language` (config-driven)
/// and concrete `Chunker` implementations. Callers must go through
/// `chunker_for` rather than instantiating chunkers directly so that
/// adding a language is a single-site change.
pub fn chunker_for(language: &Language) -> Box<dyn Chunker + Send + Sync> {
    match language {
        Language::Rust => Box::new(RustChunker),
        Language::Cpp => Box::new(CppChunker),
    }
}
