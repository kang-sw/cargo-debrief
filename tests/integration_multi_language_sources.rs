//! Contract-joint tests for the multi-language sources epic
//! (`260409-epic-multi-language-sources`).
//!
//! These tests exercise the seams defined in the skeleton:
//! 1. `DebriefService::list_sources` backward-compat auto-detection:
//!    a project root with no `.debrief/config.toml` but a `Cargo.toml`
//!    must yield a non-empty `Vec<SourceEntry>`.
//! 2. `chunker::chunker_for(Language::Rust)` returns a chunker that
//!    successfully chunks a trivial Rust snippet — the dispatch point
//!    is live and correctly routes to `RustChunker`.

use std::path::Path;
use std::process::Command;

use cargo_debrief::{
    chunker::chunker_for,
    config::Language,
    service::{DebriefService, InProcessService},
};

// ---------------------------------------------------------------------------
// Test 1: list_sources backward compat (no config, Cargo.toml present)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_sources_auto_detects_cargo_toml_when_no_config() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    // Init a git repo so `config_paths` resolves a project path.
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()?;

    // Plant a minimal Cargo.toml — content doesn't need to be valid
    // cargo metadata; only the file's presence triggers the fallback.
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )?;

    let service = InProcessService::new();
    let sources = service.list_sources(dir.path()).await?;

    assert_eq!(
        sources.len(),
        1,
        "expected exactly one auto-detected source, got {}",
        sources.len()
    );
    assert_eq!(
        sources[0].language,
        Language::Rust,
        "auto-detected source must be Rust"
    );
    assert!(
        !sources[0].dep,
        "auto-detected source must not be marked as a dep"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Test 2: chunker_for(Rust) dispatch produces a working chunker
// ---------------------------------------------------------------------------

#[test]
fn chunker_for_rust_chunks_trivial_snippet() {
    let chunker = chunker_for(&Language::Rust);

    const SNIPPET: &str = r#"
/// A trivial function used to smoke-test the Rust chunker dispatch.
pub fn answer() -> u32 {
    42
}
"#;

    let chunks = chunker
        .chunk(Path::new("src/trivial.rs"), SNIPPET)
        .expect("chunker_for(Rust) must successfully chunk a trivial .rs snippet");

    assert!(
        !chunks.is_empty(),
        "RustChunker should produce at least one chunk for a pub fn"
    );
}
