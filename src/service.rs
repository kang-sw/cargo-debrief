use std::path::Path;

use anyhow::Result;

use crate::config::Config;

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
/// Phase 1: [`InProcessService`] executes everything in-process.
/// Phase 2: `DaemonClient` will send requests over IPC to a background daemon.
///
/// Not object-safe (uses RPITIT). Dispatch is monomorphized at compile time.
pub trait DebriefService {
    fn index(&self, path: &Path) -> impl Future<Output = Result<IndexResult>> + Send;

    fn search(
        &self,
        query: &str,
        top_k: usize,
    ) -> impl Future<Output = Result<Vec<SearchResult>>> + Send;

    fn get_skeleton(&self, file: &Path) -> impl Future<Output = Result<String>> + Send;

    fn set_embedding_model(
        &self,
        model: &str,
        global: bool,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// In-process service implementation. Executes all operations directly
/// in the CLI process without a daemon.
pub struct InProcessService {
    #[allow(dead_code)]
    config: Config,
}

impl InProcessService {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl DebriefService for InProcessService {
    async fn index(&self, path: &Path) -> Result<IndexResult> {
        anyhow::bail!("not yet implemented: index {}", path.display())
    }

    async fn search(&self, query: &str, top_k: usize) -> Result<Vec<SearchResult>> {
        anyhow::bail!("not yet implemented: search {query:?} (top_k={top_k})")
    }

    async fn get_skeleton(&self, file: &Path) -> Result<String> {
        anyhow::bail!("not yet implemented: get-skeleton {}", file.display())
    }

    async fn set_embedding_model(&self, model: &str, global: bool) -> Result<()> {
        anyhow::bail!("not yet implemented: set-embedding-model {model:?} (global={global})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_process_service_stubs_return_errors() {
        let service = InProcessService::new(Config::default());

        let err = service.index(Path::new(".")).await.unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));

        let err = service.search("foo", 10).await.unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));

        let err = service
            .get_skeleton(Path::new("src/main.rs"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));

        let err = service
            .set_embedding_model("test", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }
}
