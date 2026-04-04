use std::path::Path;

use anyhow::Result;

/// Result of an indexing operation.
#[derive(Debug)]
pub struct IndexResult {
    pub files_indexed: usize,
    pub chunks_created: usize,
}

/// A single search result with relevance score.
#[derive(Debug)]
pub struct SearchResult {
    pub file_path: String,
    pub line_range: (usize, usize),
    pub score: f64,
    pub display_text: String,
}

/// Service boundary between CLI and core logic.
///
/// Each method receives `project_root` explicitly, enabling a single
/// daemon instance to serve multiple workspaces without a separate
/// routing/multiplexing layer.
///
/// Phase 1: [`InProcessService`] executes everything in-process.
/// Phase 2: `DaemonClient` will send requests over IPC to a background daemon,
/// including `project_root` in each request.
///
/// Not object-safe (uses RPITIT). Dispatch is monomorphized at compile time.
pub trait DebriefService {
    fn index(
        &self,
        project_root: &Path,
        path: &Path,
    ) -> impl Future<Output = Result<IndexResult>> + Send;

    fn search(
        &self,
        project_root: &Path,
        query: &str,
        top_k: usize,
    ) -> impl Future<Output = Result<Vec<SearchResult>>> + Send;

    fn get_skeleton(
        &self,
        project_root: &Path,
        file: &Path,
    ) -> impl Future<Output = Result<String>> + Send;

    fn set_embedding_model(
        &self,
        project_root: &Path,
        model: &str,
        global: bool,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// In-process service implementation. Executes all operations directly
/// in the CLI process without a daemon. Config is resolved from the
/// `project_root` on each call.
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
    async fn index(&self, project_root: &Path, path: &Path) -> Result<IndexResult> {
        anyhow::bail!(
            "not yet implemented: index {} (root: {})",
            path.display(),
            project_root.display()
        )
    }

    async fn search(
        &self,
        project_root: &Path,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchResult>> {
        anyhow::bail!(
            "not yet implemented: search {query:?} (top_k={top_k}, root: {})",
            project_root.display()
        )
    }

    async fn get_skeleton(&self, project_root: &Path, file: &Path) -> Result<String> {
        anyhow::bail!(
            "not yet implemented: get-skeleton {} (root: {})",
            file.display(),
            project_root.display()
        )
    }

    async fn set_embedding_model(
        &self,
        project_root: &Path,
        model: &str,
        global: bool,
    ) -> Result<()> {
        anyhow::bail!(
            "not yet implemented: set-embedding-model {model:?} (global={global}, root: {})",
            project_root.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_process_service_stubs_return_errors() {
        let service = InProcessService::new();
        let root = Path::new(".");

        let err = service.index(root, Path::new(".")).await.unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));

        let err = service.search(root, "foo", 10).await.unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));

        let err = service
            .get_skeleton(root, Path::new("src/main.rs"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));

        let err = service
            .set_embedding_model(root, "test", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[tokio::test]
    async fn in_process_service_different_roots_are_independent() {
        let service = InProcessService::new();

        let err_a = service
            .index(Path::new("/tmp/project-a"), Path::new("."))
            .await
            .unwrap_err();
        let err_b = service
            .index(Path::new("/tmp/project-b"), Path::new("."))
            .await
            .unwrap_err();

        // Both are stub errors; roots are passed through independently.
        assert!(err_a.to_string().contains("not yet implemented"));
        assert!(err_b.to_string().contains("not yet implemented"));
        assert!(err_a.to_string().contains("project-a"));
        assert!(err_b.to_string().contains("project-b"));
    }
}
