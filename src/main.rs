use std::path::PathBuf;

use anyhow::Result;
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
    let config_paths = cargo_debrief::config::config_paths(&project_root);
    let _config = cargo_debrief::config::load_config(&config_paths)?;

    match cli.command {
        Command::Index { path } => {
            eprintln!("index: not yet implemented (path: {})", path.display());
        }
        Command::Search { query, top_k } => {
            eprintln!("search: not yet implemented (query: {query:?}, top_k: {top_k})");
        }
        Command::GetSkeleton { file } => {
            eprintln!(
                "get-skeleton: not yet implemented (file: {})",
                file.display()
            );
        }
        Command::SetEmbeddingModel { model, global } => {
            eprintln!(
                "set-embedding-model: not yet implemented (model: {model:?}, global: {global})"
            );
        }
    }

    Ok(())
}
