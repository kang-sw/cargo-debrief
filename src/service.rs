//! Service boundary between CLI and core logic.
//!
//! Skeleton rewrite for the multi-language sources epic
//! (`260409-epic-multi-language-sources`). The full config-driven
//! pipeline lands during Phase 1 implementation; the bodies below are
//! `todo!()` placeholders anchoring the public contract.
//!
//! Preserved from the previous implementation:
//! - `IndexResult` / `SearchResult` public types (wire format shared
//!   with `ipc/protocol.rs` and `search.rs`).
//! - The five existing `DebriefService` trait methods (daemon.rs and
//!   the IPC protocol still dispatch through these — signatures
//!   are frozen).
//!
//! New in this skeleton:
//! - Three `DebriefService` methods for source registration
//!   (`add_source`, `list_sources`, `remove_source`) backing the
//!   new CLI subcommands. Config-only; no indexing.

use std::path::Path;

use anyhow::Result;

use crate::chunk::ChunkOrigin;
use crate::config::{self, Language, SourceEntry};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Result of an indexing operation.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IndexResult {
    pub files_indexed: usize,
    pub chunks_created: usize,
}

/// A single search result with relevance score.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub file_path: String,
    pub line_range: (usize, usize),
    pub score: f64,
    pub display_text: String,
    pub module_path: String,
    pub origin: ChunkOrigin,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Service boundary between CLI and core logic.
///
/// Each method receives `project_root` explicitly, enabling a single
/// daemon instance to serve multiple workspaces.
///
/// Not object-safe (uses RPITIT). Dispatch is monomorphized at compile time.
pub trait DebriefService {
    // --- Indexing / retrieval (existing shape, preserved) -----------------

    fn index(
        &self,
        project_root: &Path,
        path: &Path,
        include_deps: bool,
    ) -> impl Future<Output = Result<IndexResult>> + Send;

    fn search(
        &self,
        project_root: &Path,
        query: &str,
        top_k: usize,
        include_deps: bool,
    ) -> impl Future<Output = Result<Vec<SearchResult>>> + Send;

    fn overview(
        &self,
        project_root: &Path,
        file: &Path,
    ) -> impl Future<Output = Result<String>> + Send;

    fn dep_overview(
        &self,
        project_root: &Path,
        crate_name: &str,
    ) -> impl Future<Output = Result<String>> + Send;

    fn set_embedding_model(
        &self,
        project_root: &Path,
        model: &str,
        global: bool,
    ) -> impl Future<Output = Result<()>> + Send;

    // --- Source registration (new) -----------------------------------------

    /// Append a source entry to the project config.
    fn add_source(
        &self,
        project_root: &Path,
        language: Language,
        root: &Path,
        dep: bool,
    ) -> impl Future<Output = Result<()>> + Send;

    /// List registered sources for the project, applying the
    /// Cargo.toml backward-compat fallback when the config has none.
    fn list_sources(
        &self,
        project_root: &Path,
    ) -> impl Future<Output = Result<Vec<SourceEntry>>> + Send;

    /// Remove the source entry at `index` from the project config.
    fn remove_source(
        &self,
        project_root: &Path,
        index: usize,
    ) -> impl Future<Output = Result<()>> + Send;
}

// ---------------------------------------------------------------------------
// InProcessService
// ---------------------------------------------------------------------------

/// In-process service implementation. Executes all operations directly
/// in the CLI process without a daemon.
pub struct InProcessService;

impl InProcessService {
    pub fn new() -> Self {
        Self
    }
}

impl Default for InProcessService {
    fn default() -> Self {
        Self::new()
    }
}

impl DebriefService for InProcessService {
    async fn index(
        &self,
        _project_root: &Path,
        _path: &Path,
        _include_deps: bool,
    ) -> Result<IndexResult> {
        todo!("InProcessService::index — config-driven pipeline lands in Phase 1")
    }

    async fn search(
        &self,
        _project_root: &Path,
        _query: &str,
        _top_k: usize,
        _include_deps: bool,
    ) -> Result<Vec<SearchResult>> {
        todo!("InProcessService::search — config-driven pipeline lands in Phase 1")
    }

    async fn overview(&self, _project_root: &Path, _file: &Path) -> Result<String> {
        todo!("InProcessService::overview — config-driven pipeline lands in Phase 1")
    }

    async fn dep_overview(&self, _project_root: &Path, _crate_name: &str) -> Result<String> {
        todo!("InProcessService::dep_overview — dep indexing lands in Phase 3")
    }

    async fn set_embedding_model(
        &self,
        _project_root: &Path,
        _model: &str,
        _global: bool,
    ) -> Result<()> {
        todo!("InProcessService::set_embedding_model — wired during Phase 1")
    }

    async fn add_source(
        &self,
        project_root: &Path,
        language: Language,
        root: &Path,
        dep: bool,
    ) -> Result<()> {
        let entry = SourceEntry {
            language,
            root: root.to_path_buf(),
            dep,
            extensions: None,
        };
        config::append_source(project_root, entry)
    }

    async fn list_sources(&self, project_root: &Path) -> Result<Vec<SourceEntry>> {
        // Real implementation: honors the backward-compat Cargo.toml
        // fallback so the skeleton integration test has something to
        // exercise. See `config::resolve_sources`.
        config::resolve_sources(project_root)
    }

    async fn remove_source(&self, project_root: &Path, index: usize) -> Result<()> {
        config::remove_source_at(project_root, index)
    }
}

// ---------------------------------------------------------------------------
// DaemonClient
// ---------------------------------------------------------------------------

/// Client that communicates with a running daemon process via IPC.
///
/// Skeleton: trait method bodies are `todo!()`. The connection helper
/// is preserved because `daemon.rs` and `main.rs` import it and their
/// code paths must compile.
pub struct DaemonClient {
    daemon_dir: std::path::PathBuf,
    timeout: std::time::Duration,
}

impl DaemonClient {
    /// Connect to an existing daemon for the given project root.
    /// Returns `None` if no daemon is running.
    pub fn connect(project_root: &Path) -> Option<Self> {
        let dir = crate::daemon::daemon_dir(project_root).ok()?;
        if !crate::daemon::is_daemon_running(&dir) {
            return None;
        }

        // Debug builds: check binary identity to detect stale daemon after recompile.
        if !crate::daemon::check_binary_identity(&dir) {
            eprintln!("[daemon] binary mismatch, killing stale daemon");
            crate::daemon::kill_stale_daemon(&dir);
            return None;
        }

        let ready = crate::ipc::ready_indicator(&dir);
        if !ready.exists() {
            return None;
        }

        Some(Self {
            daemon_dir: dir,
            timeout: std::time::Duration::from_secs(120),
        })
    }

    #[allow(dead_code)]
    fn send(
        &self,
        request: crate::ipc::protocol::DaemonRequest,
    ) -> Result<crate::ipc::protocol::DaemonResponse> {
        crate::ipc::send_command(&self.daemon_dir, request, self.timeout)
    }
}

impl DebriefService for DaemonClient {
    async fn index(
        &self,
        _project_root: &Path,
        _path: &Path,
        _include_deps: bool,
    ) -> Result<IndexResult> {
        todo!("DaemonClient::index — IPC dispatch rewired in Phase 1")
    }

    async fn search(
        &self,
        _project_root: &Path,
        _query: &str,
        _top_k: usize,
        _include_deps: bool,
    ) -> Result<Vec<SearchResult>> {
        todo!("DaemonClient::search — IPC dispatch rewired in Phase 1")
    }

    async fn overview(&self, _project_root: &Path, _file: &Path) -> Result<String> {
        todo!("DaemonClient::overview — IPC dispatch rewired in Phase 1")
    }

    async fn dep_overview(&self, _project_root: &Path, _crate_name: &str) -> Result<String> {
        todo!("DaemonClient::dep_overview — IPC dispatch rewired in Phase 3")
    }

    async fn set_embedding_model(
        &self,
        _project_root: &Path,
        _model: &str,
        _global: bool,
    ) -> Result<()> {
        todo!("DaemonClient::set_embedding_model — IPC dispatch rewired in Phase 1")
    }

    async fn add_source(
        &self,
        _project_root: &Path,
        _language: Language,
        _root: &Path,
        _dep: bool,
    ) -> Result<()> {
        todo!("DaemonClient::add_source — IPC dispatch added in Phase 1")
    }

    async fn list_sources(&self, _project_root: &Path) -> Result<Vec<SourceEntry>> {
        todo!("DaemonClient::list_sources — IPC dispatch added in Phase 1")
    }

    async fn remove_source(&self, _project_root: &Path, _index: usize) -> Result<()> {
        todo!("DaemonClient::remove_source — IPC dispatch added in Phase 1")
    }
}

// ---------------------------------------------------------------------------
// Service — daemon-first dispatch with in-process fallback
// ---------------------------------------------------------------------------

/// Unified service dispatch: tries daemon first, falls back to in-process
/// on any IPC transport failure.
pub struct Service {
    daemon: Option<DaemonClient>,
    in_process: InProcessService,
}

impl Service {
    /// Create a service with both daemon and in-process backends.
    /// If daemon is `None`, all requests go directly to in-process.
    pub fn new(daemon: Option<DaemonClient>) -> Self {
        Self {
            daemon,
            in_process: InProcessService::new(),
        }
    }
}

impl DebriefService for Service {
    async fn index(
        &self,
        project_root: &Path,
        path: &Path,
        include_deps: bool,
    ) -> Result<IndexResult> {
        if let Some(d) = &self.daemon {
            match d.index(project_root, path, include_deps).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process
            .index(project_root, path, include_deps)
            .await
    }

    async fn search(
        &self,
        project_root: &Path,
        query: &str,
        top_k: usize,
        include_deps: bool,
    ) -> Result<Vec<SearchResult>> {
        if let Some(d) = &self.daemon {
            match d.search(project_root, query, top_k, include_deps).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process
            .search(project_root, query, top_k, include_deps)
            .await
    }

    async fn overview(&self, project_root: &Path, file: &Path) -> Result<String> {
        if let Some(d) = &self.daemon {
            match d.overview(project_root, file).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process.overview(project_root, file).await
    }

    async fn dep_overview(&self, project_root: &Path, crate_name: &str) -> Result<String> {
        if let Some(d) = &self.daemon {
            match d.dep_overview(project_root, crate_name).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process.dep_overview(project_root, crate_name).await
    }

    async fn set_embedding_model(
        &self,
        project_root: &Path,
        model: &str,
        global: bool,
    ) -> Result<()> {
        if let Some(d) = &self.daemon {
            match d.set_embedding_model(project_root, model, global).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process
            .set_embedding_model(project_root, model, global)
            .await
    }

    async fn add_source(
        &self,
        project_root: &Path,
        language: Language,
        root: &Path,
        dep: bool,
    ) -> Result<()> {
        if let Some(d) = &self.daemon {
            match d.add_source(project_root, language, root, dep).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process
            .add_source(project_root, language, root, dep)
            .await
    }

    async fn list_sources(&self, project_root: &Path) -> Result<Vec<SourceEntry>> {
        if let Some(d) = &self.daemon {
            match d.list_sources(project_root).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process.list_sources(project_root).await
    }

    async fn remove_source(&self, project_root: &Path, index: usize) -> Result<()> {
        if let Some(d) = &self.daemon {
            match d.remove_source(project_root, index).await {
                Ok(r) => return Ok(r),
                Err(e) => eprintln!("[daemon] error, falling back to in-process: {e}"),
            }
        }
        self.in_process.remove_source(project_root, index).await
    }
}
