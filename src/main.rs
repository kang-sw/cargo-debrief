use std::path::PathBuf;

use anyhow::Result;
use cargo_debrief::{
    chunk::ChunkOrigin,
    daemon,
    ipc::protocol::{DaemonRequest, DaemonResponse},
    service::{DebriefService, Service},
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargo-debrief", about = "RAG-based code retrieval for LLMs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Perform a manual full re-index of the codebase (normally implicit)
    #[command(name = "rebuild-index")]
    RebuildIndex,
    /// Search indexed code chunks
    Search {
        /// Search query
        query: String,
        /// Number of results to return
        #[arg(long, default_value_t = 10)]
        top_k: usize,
        /// Exclude dependency chunks from search results
        #[arg(long)]
        no_deps: bool,
    },
    /// Show file-level overview (declarations and signatures only)
    Overview {
        /// Source file path (mutually exclusive with --dep)
        file: Option<PathBuf>,
        /// Show overview of a dependency crate by name
        #[arg(long)]
        dep: Option<String>,
    },
    /// Configure the embedding model
    SetEmbeddingModel {
        /// Model name
        model: String,
        /// Set globally instead of per-project
        #[arg(long)]
        global: bool,
    },
    /// Manage the background daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Internal: daemon process entry point (not shown in help)
    #[command(name = "__daemon", hide = true)]
    DaemonEntry {
        /// Project root to serve
        #[arg(long)]
        project_root: PathBuf,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Check if daemon is running, report PID and uptime
    Status,
    /// Stop the running daemon
    Stop,
}

/// Resolve the service backend: try daemon (auto-spawn if needed), fall back to in-process.
fn resolve_service(project_root: &std::path::Path) -> Service {
    let client = daemon::auto_spawn_and_connect(project_root);
    Service::new(client)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let project_root = std::env::current_dir()?;

    match cli.command {
        Command::DaemonEntry { project_root } => {
            return daemon::run_daemon(&project_root).await;
        }
        Command::Daemon { action } => {
            return handle_daemon_action(&project_root, action);
        }
        _ => {}
    }

    let service = resolve_service(&project_root);

    match cli.command {
        Command::RebuildIndex => {
            let result = service.index(&project_root, &project_root).await?;
            println!(
                "Indexed {} files, {} chunks created.",
                result.files_indexed, result.chunks_created
            );
        }
        Command::Search {
            query,
            top_k,
            no_deps,
        } => {
            let results = service
                .search(&project_root, &query, top_k, !no_deps)
                .await?;
            for (i, r) in results.iter().enumerate() {
                let dep_label = match &r.origin {
                    ChunkOrigin::Dependency { crate_name, .. } => {
                        format!(" [dep: {crate_name}]")
                    }
                    ChunkOrigin::Project => String::new(),
                };
                println!(
                    "#{} [score: {:.4}]{} {}:{}-{}",
                    i + 1,
                    r.score,
                    dep_label,
                    r.file_path,
                    r.line_range.0,
                    r.line_range.1,
                );
                if !r.module_path.is_empty() {
                    println!("// in {}", r.module_path);
                }
                println!("{}", r.display_text);
                println!();
            }
        }
        Command::Overview { file, dep } => match (file, dep) {
            (Some(f), None) => {
                let skeleton = service.overview(&project_root, &f).await?;
                println!("{skeleton}");
            }
            (None, Some(crate_name)) => {
                let skeleton = service.dep_overview(&project_root, &crate_name).await?;
                println!("{skeleton}");
            }
            _ => anyhow::bail!("provide exactly one of a file path or --dep <crate-name>"),
        },
        Command::SetEmbeddingModel { model, global } => {
            service
                .set_embedding_model(&project_root, &model, global)
                .await?;
            let scope = if global { "global" } else { "project" };
            println!("Embedding model set to {model:?} ({scope}).");
        }
        // Already handled above
        Command::DaemonEntry { .. } | Command::Daemon { .. } => unreachable!(),
    }

    Ok(())
}

fn handle_daemon_action(project_root: &std::path::Path, action: DaemonAction) -> Result<()> {
    let dir = daemon::daemon_dir(project_root)?;

    match action {
        DaemonAction::Status => {
            if !daemon::is_daemon_running(&dir) {
                println!("Daemon is not running.");
                return Ok(());
            }

            // Try to get status via IPC
            match cargo_debrief::ipc::send_command(
                &dir,
                DaemonRequest::Status,
                std::time::Duration::from_secs(5),
            ) {
                Ok(DaemonResponse::Status { pid, uptime_secs }) => {
                    println!("Daemon running (PID {pid}, uptime {uptime_secs}s)");
                }
                Ok(other) => {
                    println!("Daemon responded unexpectedly: {other:?}");
                }
                Err(e) => {
                    // Daemon PID exists but IPC failed
                    if let Some(pid) = daemon::read_pid(&dir) {
                        println!("Daemon PID {pid} exists but IPC unavailable: {e}");
                    } else {
                        println!("Daemon is not running.");
                    }
                }
            }
        }
        DaemonAction::Stop => {
            if !daemon::is_daemon_running(&dir) {
                println!("Daemon is not running.");
                return Ok(());
            }

            match cargo_debrief::ipc::send_command(
                &dir,
                DaemonRequest::Stop,
                std::time::Duration::from_secs(5),
            ) {
                Ok(DaemonResponse::Ok { message }) => {
                    println!("Daemon: {message}");
                }
                Ok(other) => {
                    println!("Unexpected response: {other:?}");
                }
                Err(e) => {
                    eprintln!("Failed to send stop: {e}");
                    // Force kill as fallback
                    daemon::kill_stale_daemon(&dir);
                    println!("Daemon forcefully stopped.");
                }
            }
        }
    }

    Ok(())
}
