use std::path::PathBuf;

use anyhow::Result;
use cargo_debrief::service::{DebriefService, InProcessService};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargo-debrief", about = "RAG-based code retrieval for LLMs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Index a codebase for search
    Index {
        /// Path to index (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Search indexed code chunks
    Search {
        /// Search query
        query: String,
        /// Number of results to return
        #[arg(long, default_value_t = 10)]
        top_k: usize,
    },
    /// Show file-level overview (declarations and signatures only)
    GetSkeleton {
        /// Source file path
        file: PathBuf,
    },
    /// Configure the embedding model
    SetEmbeddingModel {
        /// Model name
        model: String,
        /// Set globally instead of per-project
        #[arg(long)]
        global: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let project_root = std::env::current_dir()?;
    let service = InProcessService::new();

    match cli.command {
        Command::Index { path } => {
            let result = service.index(&project_root, &path).await?;
            println!(
                "Indexed {} files, {} chunks created.",
                result.files_indexed, result.chunks_created
            );
        }
        Command::Search { query, top_k } => {
            let results = service.search(&project_root, &query, top_k).await?;
            for (i, r) in results.iter().enumerate() {
                println!(
                    "#{} [score: {:.4}] {}:{}-{}",
                    i + 1,
                    r.score,
                    r.file_path,
                    r.line_range.0,
                    r.line_range.1,
                );
                println!("{}", r.display_text);
                println!();
            }
        }
        Command::GetSkeleton { file } => {
            let skeleton = service.get_skeleton(&project_root, &file).await?;
            println!("{skeleton}");
        }
        Command::SetEmbeddingModel { model, global } => {
            service
                .set_embedding_model(&project_root, &model, global)
                .await?;
            let scope = if global { "global" } else { "project" };
            println!("Embedding model set to {model:?} ({scope}).");
        }
    }

    Ok(())
}
