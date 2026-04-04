use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Result;
use cargo_metadata::PackageId;

/// Information about one non-workspace dependency package as returned by
/// `cargo metadata`, enriched with root-dependency reachability.
#[derive(Debug, Clone, PartialEq)]
pub struct DepPackageInfo {
    /// Crate name (matches `package.name` in `cargo metadata`).
    pub crate_name: String,
    /// Semver version string.
    pub crate_version: String,
    /// Directory containing the package's `Cargo.toml`
    /// (i.e. `manifest_path.parent()`).
    /// Phase 2 will look for `.rs` files under `src_root/src/`.
    pub src_root: PathBuf,
    /// Names of the workspace's direct dependencies from which this
    /// package is transitively reachable. Sorted, deduplicated.
    pub root_deps: Vec<String>,
}

/// Run `cargo metadata` in `project_root` and return one `DepPackageInfo`
/// for every non-workspace package in the resolved dependency graph.
///
/// Shells out to `cargo metadata --no-deps=false --format-version 1`.
/// The caller is responsible for caching (Phase 2 concern).
///
/// # Errors
/// Returns `Err` if `cargo` is not found, exits non-zero, or the JSON
/// cannot be parsed.
pub fn discover_dependency_packages(project_root: &Path) -> Result<Vec<DepPackageInfo>> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .current_dir(project_root)
        .exec()?;

    let workspace_member_ids: HashSet<&PackageId> = metadata.workspace_members.iter().collect();

    let resolve = metadata.resolve.as_ref().expect("resolve graph missing");

    // Build adjacency map: PackageId -> Vec<PackageId>
    let adjacency: HashMap<&PackageId, Vec<&PackageId>> = resolve
        .nodes
        .iter()
        .map(|node| {
            let deps: Vec<&PackageId> = node.deps.iter().map(|d| &d.pkg).collect();
            (&node.id, deps)
        })
        .collect();

    // Collect root dep seeds: direct deps of workspace members (by PackageId -> name)
    let mut root_dep_seeds: HashMap<&PackageId, String> = HashMap::new();
    for node in &resolve.nodes {
        if workspace_member_ids.contains(&node.id) {
            for node_dep in &node.deps {
                root_dep_seeds
                    .entry(&node_dep.pkg)
                    .or_insert_with(|| node_dep.name.replace('-', "_"));
            }
        }
    }

    // BFS: for each seed, propagate its name to all reachable packages
    let mut reachable: HashMap<&PackageId, BTreeSet<String>> = HashMap::new();

    // Initialise: seeds reach themselves
    for (seed_id, seed_name) in &root_dep_seeds {
        reachable
            .entry(seed_id)
            .or_default()
            .insert(seed_name.clone());
    }

    for (seed_id, seed_name) in &root_dep_seeds {
        let mut queue = VecDeque::from([*seed_id]);
        let mut visited: HashSet<&PackageId> = HashSet::new();
        while let Some(current) = queue.pop_front() {
            if !visited.insert(current) {
                continue;
            }
            for dep_id in adjacency.get(current).into_iter().flatten() {
                reachable
                    .entry(dep_id)
                    .or_default()
                    .insert(seed_name.clone());
                queue.push_back(dep_id);
            }
        }
    }

    // Assemble result for non-workspace packages
    let mut result: Vec<DepPackageInfo> = metadata
        .packages
        .iter()
        .filter(|pkg| !workspace_member_ids.contains(&pkg.id))
        .map(|pkg| {
            let root_deps = reachable
                .get(&pkg.id)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            let src_root = pkg
                .manifest_path
                .parent()
                .expect("manifest_path has no parent")
                .into();
            DepPackageInfo {
                crate_name: pkg.name.clone(),
                crate_version: pkg.version.to_string(),
                src_root,
                root_deps,
            }
        })
        .collect();

    result.sort_by(|a, b| a.crate_name.cmp(&b.crate_name));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_dependency_packages_on_self() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = discover_dependency_packages(manifest_dir)
            .expect("discover_dependency_packages failed");

        assert!(!result.is_empty(), "expected at least one dependency");

        assert!(
            result.iter().all(|p| p.crate_name != "cargo-debrief"),
            "workspace member should be excluded"
        );

        let known_dep_names = ["anyhow", "serde", "tokio"];
        assert!(
            known_dep_names
                .iter()
                .any(|name| result.iter().any(|p| &p.crate_name == name)),
            "expected at least one of {:?} in results",
            known_dep_names
        );

        for entry in &result {
            assert!(
                !entry.root_deps.is_empty(),
                "package {} has empty root_deps",
                entry.crate_name
            );
        }

        if let Some(anyhow_entry) = result.iter().find(|p| p.crate_name == "anyhow") {
            assert!(
                anyhow_entry.src_root.exists(),
                "src_root for anyhow does not exist: {:?}",
                anyhow_entry.src_root
            );
        }
    }

    #[test]
    fn test_root_deps_sorted_deduplicated() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = discover_dependency_packages(manifest_dir)
            .expect("discover_dependency_packages failed");

        // serde is a direct dep; find it and verify root_deps contains "serde" and is sorted
        let serde_entry = result
            .iter()
            .find(|p| p.crate_name == "serde")
            .expect("serde should be in results");

        assert!(
            serde_entry.root_deps.contains(&"serde".to_string()),
            "serde's root_deps should contain 'serde'"
        );

        // Verify sorted
        let mut sorted = serde_entry.root_deps.clone();
        sorted.sort();
        assert_eq!(serde_entry.root_deps, sorted, "root_deps should be sorted");

        // Verify deduplicated
        let unique_count = serde_entry.root_deps.iter().collect::<HashSet<_>>().len();
        assert_eq!(
            serde_entry.root_deps.len(),
            unique_count,
            "root_deps should be deduplicated"
        );
    }
}
