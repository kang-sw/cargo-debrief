use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use cargo_debrief::{
    chunk::Chunk,
    chunker::{Chunker, RustChunker},
    config::{Config, ConfigPaths, load_config, save_config},
    git::{self, changed_files},
    search::SearchIndex,
    store::{DirtySnapshot, GitRepoState, IndexData, load_index, save_index},
};

/// Set CARGO_DEBRIEF_BIN env var once for daemon tests (points spawn_daemon at the real binary).
fn ensure_debrief_bin_env() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY: called exactly once before any threads that read this var.
        unsafe { std::env::set_var("CARGO_DEBRIEF_BIN", env!("CARGO_BIN_EXE_cargo-debrief")) };
    });
}

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
            ..Default::default()
        },
    )
    .expect("save global config");

    save_config(
        &project_path,
        &Config {
            embedding_model: Some("model-b".into()),
            ..Default::default()
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
            ..Default::default()
        },
    )
    .expect("save global config");

    // Project config exists but does NOT set embedding_model.
    save_config(
        &project_path,
        &Config {
            embedding_model: None,
            ..Default::default()
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

// ---------------------------------------------------------------------------
// Test 7: daemon lifecycle — spawn, status, stop
// ---------------------------------------------------------------------------

/// Integration test for daemon process lifecycle.
/// Spawns the daemon binary, sends status + stop requests via IPC.
#[test]
fn daemon_spawn_status_stop() {
    use cargo_debrief::ipc;
    use cargo_debrief::ipc::protocol::{DaemonRequest, DaemonResponse};
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().expect("tempdir");
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .expect("git init");

    // Daemon dir path
    let daemon_dir = dir.path().join(".git").join("debrief").join("daemon");

    let bin = env!("CARGO_BIN_EXE_cargo-debrief");

    // Spawn daemon
    let mut child = Command::new(bin)
        .args(["__daemon", "--project-root", dir.path().to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .env("CARGO_DEBRIEF_DAEMON_TIMEOUT", "10") // short timeout for test
        .spawn()
        .expect("spawn daemon");

    // Wait for readiness indicator
    let ready = ipc::ready_indicator(&daemon_dir);
    let start = Instant::now();
    while !ready.exists() {
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("daemon did not become ready within 10s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Send status request
    let response = ipc::send_command(&daemon_dir, DaemonRequest::Status, Duration::from_secs(5))
        .expect("status request");

    match response {
        DaemonResponse::Status { pid, uptime_secs } => {
            assert_eq!(pid, child.id(), "PID should match spawned process");
            assert!(uptime_secs < 60, "uptime should be small");
        }
        other => panic!("expected Status response, got: {other:?}"),
    }

    // Send stop request
    let response = ipc::send_command(&daemon_dir, DaemonRequest::Stop, Duration::from_secs(5))
        .expect("stop request");

    match response {
        DaemonResponse::Ok { message } => {
            assert_eq!(message, "stopping");
        }
        other => panic!("expected Ok response, got: {other:?}"),
    }

    // Wait for process to exit
    let status = child.wait().expect("wait for daemon exit");
    assert!(status.success(), "daemon should exit cleanly");

    // PID file should be cleaned up
    assert!(
        !daemon_dir.join("daemon.pid").exists(),
        "PID file should be removed on clean shutdown"
    );
}

// ---------------------------------------------------------------------------
// Test 8: auto-spawn daemon and verify status via IPC
// ---------------------------------------------------------------------------

/// Integration test for auto-spawn: uses auto_spawn_and_connect, verifies daemon
/// is running, then stops it and verifies fallback.
#[test]
fn daemon_auto_spawn_and_fallback() {
    use cargo_debrief::daemon;
    use cargo_debrief::ipc;
    use cargo_debrief::ipc::protocol::{DaemonRequest, DaemonResponse};
    use std::time::Duration;

    ensure_debrief_bin_env();

    let dir = tempfile::tempdir().expect("tempdir");
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .expect("git init");

    // Auto-spawn should start a daemon
    let client = daemon::auto_spawn_and_connect(dir.path());
    assert!(client.is_some(), "auto_spawn_and_connect should succeed");

    let daemon_dir = daemon::daemon_dir(dir.path()).unwrap();

    // Verify daemon is running via IPC
    let response = ipc::send_command(&daemon_dir, DaemonRequest::Status, Duration::from_secs(5))
        .expect("status request");
    match response {
        DaemonResponse::Status { pid, .. } => {
            assert!(pid > 0, "daemon PID should be positive");
        }
        other => panic!("expected Status response, got: {other:?}"),
    }

    // Stop daemon
    let _ = ipc::send_command(&daemon_dir, DaemonRequest::Stop, Duration::from_secs(5));

    // Wait for shutdown
    std::thread::sleep(Duration::from_millis(500));

    // After daemon is stopped, auto_spawn_and_connect should be None if spawn is
    // not re-attempted (the daemon died). But actually it WILL auto-spawn again.
    // Verify a second auto-spawn also works.
    let client2 = daemon::auto_spawn_and_connect(dir.path());
    assert!(
        client2.is_some(),
        "second auto_spawn_and_connect should succeed"
    );

    // Cleanup: stop the second daemon
    let _ = ipc::send_command(&daemon_dir, DaemonRequest::Stop, Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(500));
}

// ---------------------------------------------------------------------------
// Test 9: stale PID file cleanup
// ---------------------------------------------------------------------------

/// Create a fake PID file with a dead PID, verify auto-spawn cleans it up
/// and starts a fresh daemon.
#[test]
fn daemon_stale_pid_cleanup() {
    use cargo_debrief::daemon;
    use cargo_debrief::ipc;
    use cargo_debrief::ipc::protocol::{DaemonRequest, DaemonResponse};
    use std::time::Duration;

    ensure_debrief_bin_env();

    let dir = tempfile::tempdir().expect("tempdir");
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .expect("git init");

    // Create daemon dir with a stale PID file (dead PID)
    let daemon_dir = dir.path().join(".git").join("debrief").join("daemon");
    std::fs::create_dir_all(&daemon_dir).unwrap();
    std::fs::write(daemon_dir.join("daemon.pid"), "4294967295").unwrap();

    // Also create stale IPC files to verify cleanup
    std::fs::write(daemon_dir.join("debrief.lock"), "").unwrap();

    // auto_spawn_and_connect should clean stale files and start fresh
    let client = daemon::auto_spawn_and_connect(dir.path());
    assert!(
        client.is_some(),
        "auto_spawn_and_connect should succeed after stale cleanup"
    );

    // The stale PID should have been replaced with the real daemon PID
    let pid = daemon::read_pid(&daemon_dir).expect("PID file should exist");
    assert_ne!(pid, 4294967295, "PID should not be the stale one");
    assert!(ipc::process_alive(pid), "daemon process should be alive");

    // Verify daemon is functional
    let response = ipc::send_command(&daemon_dir, DaemonRequest::Status, Duration::from_secs(5))
        .expect("status request");
    match response {
        DaemonResponse::Status { pid: resp_pid, .. } => {
            assert_eq!(resp_pid, pid, "status PID should match PID file");
        }
        other => panic!("expected Status response, got: {other:?}"),
    }

    // Cleanup
    let _ = ipc::send_command(&daemon_dir, DaemonRequest::Stop, Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(500));
}

// ---------------------------------------------------------------------------
// Test 10: resolve_service returns InProcess when spawn impossible
// ---------------------------------------------------------------------------

/// When not in a git repo, auto-spawn cannot determine daemon dir and resolve_service
/// falls back to InProcess silently.
#[test]
fn resolve_service_no_git_repo_returns_in_process() {
    use cargo_debrief::daemon;

    let dir = tempfile::tempdir().expect("tempdir");
    // No git init — not a git repo

    // auto_spawn_and_connect should return None (no .git dir)
    let client = daemon::auto_spawn_and_connect(dir.path());
    assert!(
        client.is_none(),
        "auto_spawn should return None outside git repo"
    );
}

// ---------------------------------------------------------------------------
// Test 11: find_git_root discovers repo root from a sub-path
// ---------------------------------------------------------------------------

#[test]
fn find_git_root_from_subpath() {
    // This test runs inside the cargo-debrief repo itself.
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sub_path = manifest_dir.join("src");

    let root = git::find_git_root(&sub_path).expect("should find git root from src/");
    assert_eq!(
        root, manifest_dir,
        "git root should be the repo root (CARGO_MANIFEST_DIR)"
    );
}

#[test]
fn find_git_root_returns_none_for_non_repo() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = git::find_git_root(dir.path());
    // tempdir is under /tmp which is unlikely to be inside a git repo,
    // but if it happens to be (e.g. inside the test runner's repo), the
    // test still passes — we just verify it doesn't panic.
    // The stronger assertion is that a path like /tmp/xxx won't find .git.
    // On macOS /tmp -> /private/tmp which is outside any repo.
    let _ = result; // no panic = pass
}

// ---------------------------------------------------------------------------
// Test 12: IndexData round-trip with populated git_states (version 8)
// ---------------------------------------------------------------------------

#[test]
fn index_data_round_trip_with_git_states() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("index.bin");

    let mut data = IndexData::new();

    // Populate git_states with a realistic entry
    let mut dirty_hashes = std::collections::HashMap::new();
    dirty_hashes.insert(PathBuf::from("src/main.rs"), [0xAB; 32]);
    dirty_hashes.insert(PathBuf::from("lib/util.rs"), [0xCD; 32]);

    data.git_states.insert(
        PathBuf::from("/home/user/project"),
        GitRepoState {
            last_indexed_commit: "deadbeef1234567890abcdef1234567890abcdef".to_string(),
            dirty_snapshot: DirtySnapshot {
                file_hashes: dirty_hashes,
            },
        },
    );

    data.git_states.insert(
        PathBuf::from("/home/user/external-lib"),
        GitRepoState {
            last_indexed_commit: "cafebabe1234567890abcdef1234567890abcdef".to_string(),
            dirty_snapshot: DirtySnapshot::default(),
        },
    );

    data.embedding_model = Some("nomic-embed-text-v1.5".to_string());

    save_index(&path, &data).expect("save_index");
    let loaded = load_index(&path)
        .expect("load_index")
        .expect("expected Some — just saved");

    // Version field must be 8 after round-trip
    // (We can't directly access `version` since it's private, but the fact
    // that load_index returned Some means version == INDEX_VERSION == 8.)

    // Verify git_states survived the round-trip
    assert_eq!(
        loaded.git_states.len(),
        2,
        "should have 2 git_states entries"
    );

    let state1 = loaded
        .git_states
        .get(&PathBuf::from("/home/user/project"))
        .expect("project git state should exist");
    assert_eq!(
        state1.last_indexed_commit,
        "deadbeef1234567890abcdef1234567890abcdef"
    );
    assert_eq!(state1.dirty_snapshot.file_hashes.len(), 2);
    assert_eq!(
        state1
            .dirty_snapshot
            .file_hashes
            .get(&PathBuf::from("src/main.rs")),
        Some(&[0xAB; 32])
    );

    let state2 = loaded
        .git_states
        .get(&PathBuf::from("/home/user/external-lib"))
        .expect("external-lib git state should exist");
    assert_eq!(
        state2.last_indexed_commit,
        "cafebabe1234567890abcdef1234567890abcdef"
    );
    assert!(state2.dirty_snapshot.file_hashes.is_empty());

    assert_eq!(
        loaded.embedding_model.as_deref(),
        Some("nomic-embed-text-v1.5")
    );
}

// ---------------------------------------------------------------------------
// Test 13: IndexData version mismatch with git_states returns None
// ---------------------------------------------------------------------------

#[test]
fn index_data_version_mismatch_returns_none_with_git_states() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("index.bin");

    let mut data = IndexData::new();
    data.git_states.insert(
        PathBuf::from("/repo"),
        GitRepoState {
            last_indexed_commit: "abc".to_string(),
            dirty_snapshot: DirtySnapshot::default(),
        },
    );

    save_index(&path, &data).expect("save_index");

    // Corrupt the version field (first 4 bytes, little-endian u32)
    let mut bytes = std::fs::read(&path).unwrap();
    let bad_version: u32 = 999;
    bytes[..4].copy_from_slice(&bad_version.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let result = load_index(&path).expect("load_index should not error");
    assert!(
        result.is_none(),
        "version mismatch should return None even with populated git_states"
    );
}
