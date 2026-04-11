//! Configuration model for cargo-debrief.
//!
//! `Config` is the canonical project configuration loaded from a
//! three-layer merge (global → project → local). `[[sources]]` is the
//! single source of truth for "what to index": each entry names a
//! language, a root directory, and optional dep classification.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Source registration model
// ---------------------------------------------------------------------------

/// Supported source languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    Cpp,
}

/// One registered source directory.
///
/// `[[sources]]` entries in `config.toml` deserialize directly into this
/// struct. `extensions = None` means "use the language default extension
/// set" (e.g. `.rs` for Rust, `.cpp/.h/.hpp/...` for C++).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    pub language: Language,
    pub root: PathBuf,
    #[serde(default)]
    pub dep: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Dependency and LLM config sub-structs
// ---------------------------------------------------------------------------

/// Dependency-specific configuration.
///
/// Retained for backward compatibility with existing `[dependencies]`
/// tables in user configs. Semantics under the new `[[sources]]` model
/// are revisited during Phase 3.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DependencyConfig {
    /// Crate names to exclude from dependency indexing.
    pub exclude: Option<Vec<String>>,
}

/// External LLM configuration placeholder.
///
/// Stub — populated during the LLM chunk summarization work. Kept here
/// so `[llm]` entries in user configs do not fail to parse.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
}

// ---------------------------------------------------------------------------
// Config top-level
// ---------------------------------------------------------------------------

/// Project-level configuration, merged from all layers.
///
/// Resolution order: local > project > global > built-in default.
/// Scalar fields use `Option` so absent values in a layer don't overwrite
/// values from a lower-priority layer. `sources` is additive at load
/// time (higher layers can append but not replace individual entries —
/// full replacement semantics are TBD during implementation).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    pub embedding_model: Option<String>,
    pub dependencies: Option<DependencyConfig>,
    pub llm: Option<LlmConfig>,
    /// Registered sources. Maps directly to `[[sources]]` in TOML.
    #[serde(default)]
    pub sources: Vec<SourceEntry>,
}

impl Config {
    /// Merge `other` on top of `self`. Scalar fields present in `other`
    /// override the corresponding fields in `self`; `sources` from
    /// `other` replace `self.sources` when non-empty.
    fn merge(&mut self, other: Config) {
        if other.embedding_model.is_some() {
            self.embedding_model = other.embedding_model;
        }
        if let Some(other_deps) = other.dependencies {
            match &mut self.dependencies {
                Some(existing) => {
                    if other_deps.exclude.is_some() {
                        existing.exclude = other_deps.exclude;
                    }
                }
                None => self.dependencies = Some(other_deps),
            }
        }
        if other.llm.is_some() {
            self.llm = other.llm;
        }
        if !other.sources.is_empty() {
            self.sources = other.sources;
        }
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolved paths for the three configuration layers.
#[derive(Debug)]
pub struct ConfigPaths {
    /// `~/.config/debrief/config.toml` (or platform equivalent)
    pub global: Option<PathBuf>,
    /// `.debrief/config.toml` (git-tracked, team-shared)
    pub project: Option<PathBuf>,
    /// `.git/debrief/local-config.toml` (machine-local)
    pub local: Option<PathBuf>,
}

/// Discover the git repository root by walking parent directories.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        if current.join(".git").is_dir() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

/// Resolve configuration file paths for a project rooted at `project_root`.
pub fn config_paths(project_root: &Path) -> ConfigPaths {
    let global = dirs::config_dir().map(|d| d.join("debrief").join("config.toml"));

    let git_root = find_git_root(project_root);

    let project = git_root
        .as_ref()
        .map(|r| r.join(".debrief").join("config.toml"));
    let local = git_root.map(|r| r.join(".git").join("debrief").join("local-config.toml"));

    ConfigPaths {
        global,
        project,
        local,
    }
}

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

/// Load a single TOML config file. Returns `None` if the file does not exist.
pub fn load_layer_single(path: &Path) -> Result<Option<Config>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let config: Config = toml::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            Ok(Some(config))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Write `config` to `path` as TOML, creating parent directories if needed.
pub fn save_config(path: &Path, config: &Config) -> Result<()> {
    let contents = toml::to_string_pretty(config).context("failed to serialize config")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write config to {}", path.display()))?;
    Ok(())
}

/// Load and merge configuration from all layers.
pub fn load_config(paths: &ConfigPaths) -> Result<Config> {
    let mut config = Config::default();

    if let Some(ref path) = paths.global
        && let Some(layer) = load_layer_single(path)?
    {
        config.merge(layer);
    }

    if let Some(ref path) = paths.project
        && let Some(layer) = load_layer_single(path)?
    {
        config.merge(layer);
    }

    if let Some(ref path) = paths.local
        && let Some(layer) = load_layer_single(path)?
    {
        config.merge(layer);
    }

    Ok(config)
}

// ---------------------------------------------------------------------------
// Source registration helpers
// ---------------------------------------------------------------------------

/// Resolve the effective source list for a project.
///
/// If the merged config has a non-empty `sources` list, return it
/// verbatim. Otherwise, fall back to auto-detection:
/// `Cargo.toml` at `project_root` → single `Language::Rust` source with
/// `root = "."`. Returns an empty vec if nothing is detectable.
///
/// This is the backward-compatibility bridge from the previous
/// Cargo.toml-hardcoded pipeline to the new `[[sources]]` model.
pub fn resolve_sources(project_root: &Path) -> Result<Vec<SourceEntry>> {
    let paths = config_paths(project_root);
    let config = load_config(&paths)?;

    if !config.sources.is_empty() {
        return Ok(config.sources);
    }

    if project_root.join("Cargo.toml").is_file() {
        return Ok(vec![SourceEntry {
            language: Language::Rust,
            root: PathBuf::from("."),
            dep: false,
            extensions: None,
        }]);
    }

    Ok(Vec::new())
}

/// Append a new source entry to the project config. Creates the project
/// config file if it does not exist.
///
/// Skeleton: unimplemented. Wired in during Phase 1 implementation.
pub fn append_source(_project_root: &Path, _entry: SourceEntry) -> Result<()> {
    todo!("append_source: load project config, push SourceEntry, save_config")
}

/// Remove the source entry at `index` from the project config.
///
/// Skeleton: unimplemented. Wired in during Phase 1 implementation.
pub fn remove_source_at(_project_root: &Path, _index: usize) -> Result<()> {
    todo!("remove_source_at: load project config, validate index, remove, save_config")
}
