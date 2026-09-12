use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ignore::WalkBuilder;
use serde::Deserialize;

/// A source file selected for analysis. Cargo target context marks integration
/// tests and benchmarks before syntax-level `cfg(test)` classification runs.
#[derive(Clone, Debug)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub is_test: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub files: Vec<DiscoveredFile>,
    pub test_files: usize,
}

#[derive(Debug)]
pub struct DiscoveryError {
    message: String,
}

impl DiscoveryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for DiscoveryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DiscoveryError {}

/// Discover Rust source files from an explicit file/directory or a Cargo
/// package/workspace root. Cargo metadata supplies exact package targets when
/// available. A deterministic filesystem walk is used for lightweight source
/// trees that do not contain a Cargo manifest.
pub fn discover(input: &Path) -> Result<Discovery, DiscoveryError> {
    let path = fs::canonicalize(input).map_err(|error| {
        DiscoveryError::new(format!("cannot access '{}': {error}", input.display()))
    })?;

    if path.is_file() {
        if !is_rust_file(&path) {
            return Err(DiscoveryError::new(format!(
                "'{}' is not a Rust source file",
                input.display()
            )));
        }

        return Ok(Discovery {
            files: vec![DiscoveredFile {
                path,
                is_test: false,
            }],
            test_files: 0,
        });
    }

    if !path.is_dir() {
        return Err(DiscoveryError::new(format!(
            "'{}' is neither a file nor a directory",
            input.display()
        )));
    }

    if path.join("Cargo.toml").is_file() {
        return discover_cargo_targets(&path);
    }

    discover_filesystem(&path)
        .map_err(|error| DiscoveryError::new(format!("cannot scan '{}': {error}", input.display())))
}

fn discover_cargo_targets(root: &Path) -> Result<Discovery, DiscoveryError> {
    let manifest = root.join("Cargo.toml");
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .current_dir(root)
        .output()
        .map_err(|error| {
            DiscoveryError::new(format!(
                "cannot run cargo metadata for '{}': {error}",
                manifest.display()
            ))
        })?;

    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let message = if details.is_empty() {
            format!("cargo metadata failed for '{}'", manifest.display())
        } else {
            format!(
                "cargo metadata failed for '{}': {details}",
                manifest.display()
            )
        };
        return Err(DiscoveryError::new(message));
    }

    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout).map_err(|error| {
        DiscoveryError::new(format!(
            "cargo metadata returned invalid JSON for '{}': {error}",
            manifest.display()
        ))
    })?;
    let mut paths = BTreeSet::new();
    let mut package_roots = BTreeSet::new();
    let mut test_target_paths = BTreeSet::new();

    for package in metadata.packages {
        let manifest_path = PathBuf::from(package.manifest_path);
        if let Some(package_root) = manifest_path.parent() {
            package_roots.insert(package_root.to_path_buf());
        }
        for target in package.targets {
            let path = absolute_path(root, PathBuf::from(target.src_path));
            if !is_rust_file(&path) {
                continue;
            }
            // Cargo target paths are authoritative, including custom targets
            // deliberately kept outside a package root or ignored by a
            // repository rule.
            paths.insert(path.clone());
            if target
                .kind
                .iter()
                .any(|kind| kind == "test" || kind == "bench")
            {
                test_target_paths.insert(path);
            }
        }
    }

    for package_root in package_roots {
        walk(&package_root, &mut paths).map_err(|error| {
            DiscoveryError::new(format!("cannot scan '{}': {error}", package_root.display()))
        })?;
    }

    let files = paths
        .into_iter()
        .map(|path| DiscoveredFile {
            is_test: is_test_path(&path) || test_target_paths.contains(&path),
            path,
        })
        .collect::<Vec<_>>();
    let test_files = files.iter().filter(|file| file.is_test).count();

    Ok(Discovery { files, test_files })
}

fn discover_filesystem(root: &Path) -> std::io::Result<Discovery> {
    let mut paths = BTreeSet::new();
    walk(root, &mut paths)?;

    Ok(Discovery {
        files: paths
            .into_iter()
            .map(|path| DiscoveredFile {
                path,
                is_test: false,
            })
            .collect(),
        test_files: 0,
    })
}

fn walk(directory: &Path, paths: &mut BTreeSet<PathBuf>) -> std::io::Result<()> {
    let mut builder = WalkBuilder::new(directory);
    builder.standard_filters(true);
    builder.filter_entry(|entry| entry.depth() == 0 || !should_skip_directory(entry.path()));

    for entry in builder.build() {
        let entry = entry.map_err(|error| std::io::Error::other(error.to_string()))?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() && is_rust_file(entry.path()) {
            paths.insert(entry.into_path());
        }
    }

    Ok(())
}

fn absolute_path(root: &Path, path: PathBuf) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    fs::canonicalize(&absolute).unwrap_or(absolute)
}

fn should_skip_directory(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        matches!(
            name.to_str(),
            Some(".git" | "target" | ".cargo" | "vendor" | "node_modules")
        )
    })
}

fn is_rust_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "rs")
}

fn is_test_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name == "tests" || name == "benches")
    })
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    manifest_path: String,
    targets: Vec<CargoTarget>,
}

#[derive(Debug, Deserialize)]
struct CargoTarget {
    kind: Vec<String>,
    src_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_test_paths_are_available_for_cargo_classification() {
        assert!(is_test_path(Path::new("crate/tests/http.rs")));
        assert!(is_test_path(Path::new("crate/benches/parser.rs")));
        assert!(!is_test_path(Path::new("crate/src/test_utils.rs")));
    }

    #[test]
    fn build_output_and_dependency_directories_are_skipped() {
        assert!(should_skip_directory(Path::new("target")));
        assert!(should_skip_directory(Path::new(".git")));
        assert!(should_skip_directory(Path::new("vendor")));
        assert!(!should_skip_directory(Path::new("src")));
    }

    #[test]
    fn ignore_files_are_respected_alongside_hard_skips() {
        let root = std::env::temp_dir().join(format!(
            "kompass-ignore-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for directory in [
            "src",
            "ignored-by-git",
            "ignored-by-ignore",
            "target",
            ".git",
            "vendor",
            "node_modules",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
            std::fs::write(root.join(directory).join("hidden.rs"), "fn hidden() {}\n").unwrap();
        }
        std::fs::write(root.join("src/keep.rs"), "fn keep() {}\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored-by-git/\n").unwrap();
        std::fs::write(root.join(".ignore"), "ignored-by-ignore/\n").unwrap();

        let discovery = discover_filesystem(&root).unwrap();
        assert_eq!(
            discovery
                .files
                .iter()
                .map(|file| file.path.strip_prefix(&root).unwrap())
                .collect::<Vec<_>>(),
            vec![Path::new("src/hidden.rs"), Path::new("src/keep.rs")]
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_metadata_failures_are_discovery_errors() {
        let root = std::env::temp_dir().join(format!(
            "kompass-invalid-cargo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package\n").unwrap();
        std::fs::write(root.join("fallback.rs"), "fn fallback() {}\n").unwrap();

        let error = discover(&root).unwrap_err();
        assert!(error.to_string().contains("cargo metadata failed"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_target_paths_outside_package_root_are_included() {
        let root = std::env::temp_dir().join(format!(
            "kompass-external-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let package = root.join("package");
        std::fs::create_dir_all(&package).unwrap();
        let external_source = root.join("shared.rs");
        std::fs::write(
            package.join("Cargo.toml"),
            "[package]\nname = \"external-target\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"../shared.rs\"\n",
        )
        .unwrap();
        std::fs::write(&external_source, "pub fn shared() {}\n").unwrap();

        let discovery = discover(&package).unwrap();
        assert!(
            discovery
                .files
                .iter()
                .any(|file| { file.path == external_source && !file.is_test })
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
