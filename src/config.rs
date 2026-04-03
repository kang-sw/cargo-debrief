use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

/// Project-level configuration, merged from all layers.
///
/// Resolution order: local > project > global > built-in default.
/// Fields use `Option` so absent values in a layer don't overwrite
/// values from a lower-priority layer.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    pub embedding_model: Option<String>,
}

impl Config {
    /// Merge `other` on top of `self`. Fields present in `other`
    /// override the corresponding fields in `self`.
    fn merge(&mut self, other: Config) {
        if other.embedding_model.is_some() {
            self.embedding_model = other.embedding_model;
        }
    }
}

/// Discover the git repository root by walking parent directories
/// looking for a `.git` directory.
///
/// Note: only detects `.git` as a directory. Git worktrees and submodules
/// use a `.git` file instead — not supported yet.
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
///
/// If `project_root` is not inside a git repository, local and project
/// paths will be `None`.
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

/// Load a single TOML config file. Returns `None` if the file does not exist.
fn load_layer(path: &Path) -> Result<Option<Config>> {
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

/// Load and merge configuration from all layers.
///
/// Resolution order: built-in default → global → project → local.
/// Each layer overrides fields set by the previous one.
pub fn load_config(paths: &ConfigPaths) -> Result<Config> {
    let mut config = Config::default();

    if let Some(ref path) = paths.global
        && let Some(layer) = load_layer(path)?
    {
        config.merge(layer);
    }

    if let Some(ref path) = paths.project
        && let Some(layer) = load_layer(path)?
    {
        config.merge(layer);
    }

    if let Some(ref path) = paths.local
        && let Some(layer) = load_layer(path)?
    {
        config.merge(layer);
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn config_paths_in_git_repo() {
        // The cargo-debrief project itself is a git repo.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let paths = config_paths(root);

        assert!(paths.global.is_some());
        assert!(paths.project.is_some());
        assert!(paths.local.is_some());

        let project = paths.project.unwrap();
        assert!(project.ends_with(".debrief/config.toml"));

        let local = paths.local.unwrap();
        assert!(local.ends_with(".git/debrief/local-config.toml"));
    }

    #[test]
    fn config_paths_outside_git_repo() {
        // /tmp is (almost certainly) not inside a git repo.
        let paths = config_paths(Path::new("/tmp"));

        assert!(paths.global.is_some());
        assert!(paths.project.is_none());
        assert!(paths.local.is_none());
    }

    #[test]
    fn merge_overrides_fields() {
        let mut base = Config {
            embedding_model: Some("base-model".into()),
        };
        let overlay = Config {
            embedding_model: Some("overlay-model".into()),
        };
        base.merge(overlay);
        assert_eq!(base.embedding_model.as_deref(), Some("overlay-model"));
    }

    #[test]
    fn merge_preserves_when_overlay_is_none() {
        let mut base = Config {
            embedding_model: Some("base-model".into()),
        };
        let overlay = Config {
            embedding_model: None,
        };
        base.merge(overlay);
        assert_eq!(base.embedding_model.as_deref(), Some("base-model"));
    }

    #[test]
    fn load_config_from_temp_layers() -> Result<()> {
        let dir = tempfile::tempdir()?;

        let global_dir = dir.path().join("global");
        fs::create_dir_all(&global_dir)?;
        fs::write(
            global_dir.join("config.toml"),
            r#"embedding_model = "global-model""#,
        )?;

        let project_dir = dir.path().join("project");
        fs::create_dir_all(&project_dir)?;
        fs::write(
            project_dir.join("config.toml"),
            r#"embedding_model = "project-model""#,
        )?;

        let paths = ConfigPaths {
            global: Some(global_dir.join("config.toml")),
            project: Some(project_dir.join("config.toml")),
            local: None,
        };

        let config = load_config(&paths)?;
        // Project overrides global.
        assert_eq!(config.embedding_model.as_deref(), Some("project-model"));
        Ok(())
    }

    #[test]
    fn load_config_missing_files_uses_default() -> Result<()> {
        let paths = ConfigPaths {
            global: Some(PathBuf::from("/nonexistent/config.toml")),
            project: None,
            local: None,
        };
        let config = load_config(&paths)?;
        assert!(config.embedding_model.is_none());
        Ok(())
    }

    #[test]
    fn load_config_malformed_toml_reports_path() {
        let dir = tempfile::tempdir().unwrap();
        let bad_file = dir.path().join("bad.toml");
        fs::write(&bad_file, "not valid toml [[[").unwrap();

        let paths = ConfigPaths {
            global: Some(bad_file.clone()),
            project: None,
            local: None,
        };
        let err = load_config(&paths).unwrap_err();
        assert!(
            err.to_string().contains("bad.toml"),
            "error should mention file path: {err}"
        );
    }
}
