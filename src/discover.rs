use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ignore::WalkBuilder;
use serde::Deserialize;

use crate::model::{Category, Language};

/// A source file selected for analysis. Cargo target context marks integration
/// tests and benchmarks before syntax-level `cfg(test)` classification runs.
#[derive(Clone, Debug)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub language: Language,
    pub category: Category,
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub files: Vec<DiscoveredFile>,
    pub test_files: usize,
    pub language_filter: LanguageFilter,
    pub explicit_file: bool,
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

/// Languages that can be selected by the command line. Explicit source files
/// always win over this filter, so `kompass --language rust script.py` still
/// analyzes the requested file. JavaScript includes `.js` and `.jsx`, while
/// TypeScript includes `.ts` and `.tsx`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LanguageFilter {
    #[default]
    All,
    Rust,
    Python,
    JavaScript,
    TypeScript,
}

impl LanguageFilter {
    pub const fn includes(self, language: Language) -> bool {
        matches!(
            (self, language),
            (Self::All, _)
                | (Self::Rust, Language::Rust)
                | (Self::Python, Language::Python)
                | (Self::JavaScript, Language::JavaScript)
                | (Self::TypeScript, Language::TypeScript)
        )
    }

    pub const fn serialized(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
        }
    }
}

/// Discover source files from an explicit file/directory or a Cargo
/// package/workspace root. Cargo metadata supplies exact Rust package targets
/// when available, while Python files are walked from the requested root so
/// scripts outside Cargo package roots remain visible.
pub fn discover(input: &Path) -> Result<Discovery, DiscoveryError> {
    discover_with_language(input, LanguageFilter::All)
}

/// Discover source files while applying a language filter to directory walks.
/// An explicit file remains authoritative and is discovered regardless of the
/// selected filter.
pub fn discover_with_language(
    input: &Path,
    language_filter: LanguageFilter,
) -> Result<Discovery, DiscoveryError> {
    let path = fs::canonicalize(input).map_err(|error| {
        DiscoveryError::new(format!("cannot access '{}': {error}", input.display()))
    })?;

    if path.is_file() {
        let Some(language) = language_for_path(&path) else {
            return Err(DiscoveryError::new(format!(
                "'{}' is not a Rust, Python, JavaScript, or TypeScript source file",
                input.display()
            )));
        };
        let category = category_for_path(&path, language);

        return Ok(Discovery {
            files: vec![DiscoveredFile {
                path,
                language,
                category,
            }],
            test_files: usize::from(category == Category::Test),
            language_filter,
            explicit_file: true,
        });
    }

    if !path.is_dir() {
        return Err(DiscoveryError::new(format!(
            "'{}' is neither a file nor a directory",
            input.display()
        )));
    }

    // A virtual-environment root is a dependency tree even when it is passed
    // directly, so keep its interpreter metadata from turning the environment
    // into an analyzed project.
    if should_skip_directory(&path) {
        return Ok(Discovery {
            language_filter,
            ..Discovery::default()
        });
    }

    if path.join("Cargo.toml").is_file() && language_filter.includes(Language::Rust) {
        return discover_cargo_targets(&path, language_filter);
    }

    discover_filesystem(&path, language_filter)
        .map_err(|error| DiscoveryError::new(format!("cannot scan '{}': {error}", input.display())))
}

fn discover_cargo_targets(
    root: &Path,
    language_filter: LanguageFilter,
) -> Result<Discovery, DiscoveryError> {
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
    let mut paths = SourcePaths::default();
    let mut package_roots = BTreeSet::new();
    let mut test_target_paths = BTreeSet::new();

    for package in metadata.packages {
        let manifest_path = PathBuf::from(package.manifest_path);
        if let Some(package_root) = manifest_path.parent() {
            package_roots.insert(package_root.to_path_buf());
        }
        for target in package.targets {
            let path = absolute_path(root, PathBuf::from(target.src_path));
            if !language_filter.includes(Language::Rust) || !is_rust_file(&path) {
                continue;
            }
            // Cargo target paths are authoritative, including custom targets
            // deliberately kept outside a package root or ignored by a
            // repository rule.
            paths.rust.insert(path.clone());
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
        let mut package_paths = SourcePaths::default();
        walk(&package_root, &mut package_paths, LanguageFilter::Rust).map_err(|error| {
            DiscoveryError::new(format!("cannot scan '{}': {error}", package_root.display()))
        })?;
        paths.rust.extend(package_paths.rust);
    }

    // Non-Rust projects commonly live beside, or outside, a Rust package. Scan
    // the requested root separately so Cargo's package-root restriction for
    // Rust does not hide Python, JavaScript, or TypeScript sources and tests.
    if language_filter.includes(Language::Python)
        || language_filter.includes(Language::JavaScript)
        || language_filter.includes(Language::TypeScript)
    {
        let mut root_paths = SourcePaths::default();
        walk(root, &mut root_paths, language_filter).map_err(|error| {
            DiscoveryError::new(format!("cannot scan '{}': {error}", root.display()))
        })?;
        paths.python.extend(root_paths.python);
        paths.javascript.extend(root_paths.javascript);
        paths.typescript.extend(root_paths.typescript);
    }

    let mut files = files_from_paths(paths, &test_target_paths);
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let test_files = files
        .iter()
        .filter(|file| file.category == Category::Test)
        .count();

    Ok(Discovery {
        files,
        test_files,
        language_filter,
        explicit_file: false,
    })
}

fn discover_filesystem(root: &Path, language_filter: LanguageFilter) -> std::io::Result<Discovery> {
    let mut paths = SourcePaths::default();
    walk(root, &mut paths, language_filter)?;

    let mut files = files_from_paths(paths, &BTreeSet::new());
    files.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(Discovery {
        test_files: files
            .iter()
            .filter(|file| file.category == Category::Test)
            .count(),
        files,
        language_filter,
        explicit_file: false,
    })
}

#[derive(Debug, Default)]
struct SourcePaths {
    rust: BTreeSet<PathBuf>,
    python: BTreeSet<PathBuf>,
    javascript: BTreeSet<PathBuf>,
    typescript: BTreeSet<PathBuf>,
}

fn files_from_paths(
    paths: SourcePaths,
    rust_test_target_paths: &BTreeSet<PathBuf>,
) -> Vec<DiscoveredFile> {
    let SourcePaths {
        rust,
        python,
        javascript,
        typescript,
    } = paths;
    let mut files = rust
        .into_iter()
        .map(|path| DiscoveredFile {
            category: if is_test_path(&path) || rust_test_target_paths.contains(&path) {
                Category::Test
            } else {
                Category::Production
            },
            language: Language::Rust,
            path,
        })
        .collect::<Vec<_>>();
    files.extend(python.into_iter().map(|path| DiscoveredFile {
        category: category_for_path(&path, Language::Python),
        language: Language::Python,
        path,
    }));
    files.extend(javascript.into_iter().map(|path| DiscoveredFile {
        category: category_for_path(&path, Language::JavaScript),
        language: Language::JavaScript,
        path,
    }));
    files.extend(typescript.into_iter().map(|path| DiscoveredFile {
        category: category_for_path(&path, Language::TypeScript),
        language: Language::TypeScript,
        path,
    }));
    files
}

fn walk(
    directory: &Path,
    paths: &mut SourcePaths,
    language_filter: LanguageFilter,
) -> std::io::Result<()> {
    let mut builder = WalkBuilder::new(directory);
    builder.standard_filters(true);
    builder.filter_entry(|entry| entry.depth() == 0 || !should_skip_directory(entry.path()));

    for entry in builder.build() {
        let entry = entry.map_err(|error| std::io::Error::other(error.to_string()))?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let path = entry.into_path();
        match language_for_path(&path) {
            Some(Language::Rust) if language_filter.includes(Language::Rust) => {
                paths.rust.insert(path);
            }
            Some(Language::Python) if language_filter.includes(Language::Python) => {
                paths.python.insert(path);
            }
            Some(Language::JavaScript)
                if language_filter.includes(Language::JavaScript)
                    && !is_excluded_source_path(&path) =>
            {
                paths.javascript.insert(path);
            }
            Some(Language::TypeScript)
                if language_filter.includes(Language::TypeScript)
                    && !is_excluded_source_path(&path) =>
            {
                paths.typescript.insert(path);
            }
            _ => {}
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
    path.join("pyvenv.cfg").is_file()
        || path.file_name().is_some_and(|name| {
            matches!(
                name.to_str(),
                Some(
                    ".git"
                        | "target"
                        | ".cargo"
                        | "vendor"
                        | "node_modules"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                        | ".tox"
                        | ".nox"
                        | "coverage"
                        | "dist"
                        | "build"
                        | "out"
                        | "generated"
                        | ".next"
                        | ".expo"
                        | ".turbo"
                        | "storybook-static"
                )
            )
        })
}

fn is_rust_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "rs")
}

fn is_python_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "py")
}

fn is_javascript_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| matches!(extension.to_str(), Some("js" | "jsx")))
}

fn is_typescript_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| matches!(extension.to_str(), Some("ts" | "tsx")))
}

fn is_javascript_family_file(path: &Path) -> bool {
    is_javascript_file(path) || is_typescript_file(path)
}

/// Generated declarations, bundles, and source trees are intentionally left
/// out of recursive source discovery. Explicit source paths retain the
/// existing authoritative-file behavior and can still be analyzed directly.
fn is_excluded_source_path(path: &Path) -> bool {
    if !is_javascript_family_file(path) {
        return false;
    }
    is_declaration_file(path) || is_bundle_file(path) || is_generated_file(path)
}

fn is_declaration_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".d.ts"))
}

fn is_bundle_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == "bundle.js"
        || name == "bundle.jsx"
        || name == "bundle.ts"
        || name == "bundle.tsx"
        || [".bundle.", ".chunk.", ".min.", ".map."]
            .iter()
            .any(|marker| name.contains(marker))
}

fn is_generated_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with("generated.")
        || [".generated.", ".gen.", ".g."]
            .iter()
            .any(|marker| name.contains(marker))
}

fn language_for_path(path: &Path) -> Option<Language> {
    if is_rust_file(path) {
        Some(Language::Rust)
    } else if is_python_file(path) {
        Some(Language::Python)
    } else if is_javascript_file(path) {
        Some(Language::JavaScript)
    } else if is_typescript_file(path) {
        Some(Language::TypeScript)
    } else {
        None
    }
}

fn category_for_path(path: &Path, language: Language) -> Category {
    if (language == Language::Python && is_python_test_path(path))
        || (language.is_javascript_family() && is_javascript_test_path(path))
    {
        Category::Test
    } else {
        Category::Production
    }
}

fn is_test_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name == "tests" || name == "benches")
    })
}

fn is_python_test_path(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name == "tests")
    }) || file_name == "conftest.py"
        || file_name.starts_with("test_") && file_name.ends_with(".py")
        || file_name.ends_with("_test.py")
}

fn is_javascript_test_path(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let path_test_directory = path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| matches!(name, "test" | "tests" | "__tests__"))
    });
    let file_test_suffix = [
        ".test.js",
        ".test.jsx",
        ".test.ts",
        ".test.tsx",
        ".spec.js",
        ".spec.jsx",
        ".spec.ts",
        ".spec.tsx",
    ]
    .iter()
    .any(|suffix| file_name.ends_with(suffix));
    let conventional_test_name = [
        "test.js",
        "test.jsx",
        "test.ts",
        "test.tsx",
        "_test.js",
        "_test.jsx",
        "_test.ts",
        "_test.tsx",
    ]
    .iter()
    .any(|suffix| file_name.ends_with(suffix))
        || file_name.starts_with("test_");
    path_test_directory || file_test_suffix || conventional_test_name
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
    fn python_test_conventions_and_language_filter_are_stable() {
        assert!(is_python_test_path(Path::new("project/tests/http.py")));
        assert!(is_python_test_path(Path::new("project/test_parser.py")));
        assert!(is_python_test_path(Path::new("project/parser_test.py")));
        assert!(is_python_test_path(Path::new("project/conftest.py")));
        assert!(!is_python_test_path(Path::new("project/src/parser.py")));
        assert!(LanguageFilter::All.includes(Language::Rust));
        assert!(LanguageFilter::All.includes(Language::Python));
        assert!(LanguageFilter::Python.includes(Language::Python));
        assert!(!LanguageFilter::Python.includes(Language::Rust));
        assert!(LanguageFilter::JavaScript.includes(Language::JavaScript));
        assert!(!LanguageFilter::JavaScript.includes(Language::TypeScript));
        assert!(LanguageFilter::TypeScript.includes(Language::TypeScript));
        assert!(!LanguageFilter::TypeScript.includes(Language::JavaScript));
        assert!(is_javascript_test_path(Path::new("src/Widget.test.jsx")));
        assert!(is_javascript_test_path(Path::new("src/Widget.spec.tsx")));
        assert!(is_javascript_test_path(Path::new("__tests__/Widget.tsx")));
        assert!(!is_javascript_test_path(Path::new("src/test-utils.ts")));
        assert_eq!(
            language_for_path(Path::new("src/App.jsx")),
            Some(Language::JavaScript)
        );
        assert_eq!(
            language_for_path(Path::new("src/App.tsx")),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn build_output_and_dependency_directories_are_skipped() {
        assert!(should_skip_directory(Path::new("target")));
        assert!(should_skip_directory(Path::new(".git")));
        assert!(should_skip_directory(Path::new("vendor")));
        assert!(should_skip_directory(Path::new("coverage")));
        assert!(should_skip_directory(Path::new(".next")));
        assert!(should_skip_directory(Path::new(".expo")));
        assert!(should_skip_directory(Path::new("generated")));
        assert!(!should_skip_directory(Path::new("ios")));
        assert!(!should_skip_directory(Path::new("android")));
        assert!(!should_skip_directory(Path::new("src")));
    }

    #[test]
    fn javascript_and_typescript_discovery_covers_react_trees_without_generated_files() {
        let root = std::env::temp_dir().join(format!(
            "kompass-javascript-discovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for directory in [
            "src",
            "ios",
            "android",
            "android/build",
            "__tests__",
            "tests",
            "node_modules/pkg",
            "coverage",
            "dist",
            ".next",
            ".expo",
            "generated",
            "ignored",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for (path, source) in [
            ("src/App.jsx", "export function App() { return null; }\n"),
            ("src/App.tsx", "export function App() { return null; }\n"),
            ("src/testHelpers.ts", "export function helper() {}\n"),
            ("ios/App.tsx", "export function NativeApp() {}\n"),
            ("android/App.tsx", "export function NativeApp() {}\n"),
            (
                "__tests__/App.test.jsx",
                "test('app', () => render(<App />));\n",
            ),
            (
                "tests/App.spec.tsx",
                "test('app', () => render(<App />));\n",
            ),
            (
                "node_modules/pkg/index.ts",
                "export function dependency() {}\n",
            ),
            ("coverage/report.ts", "export function coverage() {}\n"),
            ("dist/index.js", "export function bundle() {}\n"),
            (".next/page.tsx", "export function page() {}\n"),
            (".expo/config.ts", "export function config() {}\n"),
            ("generated/api.ts", "export function generated() {}\n"),
            ("android/build/output.ts", "export function output() {}\n"),
            ("types.d.ts", "export interface Types {}\n"),
            ("app.min.js", "function min() {}\n"),
            ("app.bundle.js", "function bundle() {}\n"),
            ("src/api.generated.ts", "export function generated() {}\n"),
            ("ignored/ignored.ts", "export function ignored() {}\n"),
        ] {
            std::fs::write(root.join(path), source).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(root.join(".ignore"), "ignored/\n").unwrap();

        let discovery = discover_with_language(&root, LanguageFilter::All).unwrap();
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let discovered = discovery
            .files
            .iter()
            .map(|file| {
                (
                    file.path
                        .strip_prefix(&canonical_root)
                        .unwrap()
                        .to_path_buf(),
                    file.language,
                    file.category,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(discovered.len(), 7);
        assert!(discovered.contains(&(
            PathBuf::from("src/App.jsx"),
            Language::JavaScript,
            Category::Production
        )));
        assert!(discovered.contains(&(
            PathBuf::from("__tests__/App.test.jsx"),
            Language::JavaScript,
            Category::Test
        )));
        assert!(discovered.contains(&(
            PathBuf::from("tests/App.spec.tsx"),
            Language::TypeScript,
            Category::Test
        )));
        assert!(discovered.contains(&(
            PathBuf::from("src/testHelpers.ts"),
            Language::TypeScript,
            Category::Production
        )));
        assert!(discovered.contains(&(
            PathBuf::from("ios/App.tsx"),
            Language::TypeScript,
            Category::Production
        )));
        assert!(discovered.contains(&(
            PathBuf::from("android/App.tsx"),
            Language::TypeScript,
            Category::Production
        )));
        assert!(discovered.contains(&(
            PathBuf::from("src/App.tsx"),
            Language::TypeScript,
            Category::Production
        )));
        assert_eq!(discovery.test_files, 2);

        let javascript = discover_with_language(&root, LanguageFilter::JavaScript).unwrap();
        assert_eq!(
            javascript
                .files
                .iter()
                .map(|file| file.language)
                .collect::<Vec<_>>(),
            vec![Language::JavaScript, Language::JavaScript]
        );
        let typescript = discover_with_language(&root, LanguageFilter::TypeScript).unwrap();
        assert_eq!(typescript.files.len(), 5);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn javascript_exclusions_are_only_applied_to_recursive_walks() {
        assert!(is_excluded_source_path(Path::new("types.d.ts")));
        assert!(is_excluded_source_path(Path::new("app.bundle.js")));
        assert!(is_excluded_source_path(Path::new("bundle.js")));
        assert!(is_excluded_source_path(Path::new("app.min.tsx")));
        assert!(is_excluded_source_path(Path::new("api.generated.ts")));
        assert!(is_excluded_source_path(Path::new("generated.ts")));
        assert!(!is_excluded_source_path(Path::new("src/App.tsx")));
        assert!(!is_excluded_source_path(Path::new("src/App.py")));
    }

    #[test]
    fn python_dependency_trees_are_skipped_and_ignored_files_remain_ignored() {
        let root = std::env::temp_dir().join(format!(
            "kompass-python-discovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for directory in [
            "src",
            "tests",
            ".venv/lib",
            "venv/lib",
            "__pycache__",
            ".tox",
            ".nox",
            "embedded-env/lib",
            "ignored",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(root.join("src/app.py"), "def app():\n    return 1\n").unwrap();
        std::fs::write(
            root.join("tests/test_app.py"),
            "def test_app():\n    pass\n",
        )
        .unwrap();
        std::fs::write(
            root.join("parser_test.py"),
            "def test_parser():\n    pass\n",
        )
        .unwrap();
        std::fs::write(root.join("conftest.py"), "def fixture():\n    pass\n").unwrap();
        for directory in [
            ".venv/lib",
            "venv/lib",
            "__pycache__",
            ".tox",
            ".nox",
            "embedded-env/lib",
            "ignored",
        ] {
            std::fs::write(
                root.join(directory).join("hidden.py"),
                "def hidden():\n    pass\n",
            )
            .unwrap();
        }
        std::fs::write(root.join("embedded-env/pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(root.join(".ignore"), "ignored/\n").unwrap();

        let discovery = discover_with_language(&root, LanguageFilter::Python).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let paths = discovery
            .files
            .iter()
            .map(|file| file.path.strip_prefix(&root).unwrap().to_path_buf())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("conftest.py"),
                PathBuf::from("parser_test.py"),
                PathBuf::from("src/app.py"),
                PathBuf::from("tests/test_app.py"),
            ]
        );
        assert!(
            discovery
                .files
                .iter()
                .all(|file| file.language == Language::Python)
        );
        assert_eq!(discovery.test_files, 3);
        assert!(
            discovery
                .files
                .iter()
                .filter(|file| file.category == Category::Test)
                .count()
                == 3
        );

        let explicit =
            discover_with_language(&root.join("src/app.py"), LanguageFilter::Rust).unwrap();
        assert_eq!(explicit.files.len(), 1);
        assert_eq!(explicit.files[0].language, Language::Python);

        std::fs::remove_dir_all(root).unwrap();
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

        let discovery = discover_filesystem(&root, LanguageFilter::All).unwrap();
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
        let external_source = std::fs::canonicalize(external_source).unwrap();
        assert!(discovery.files.iter().any(|file| {
            file.path == external_source
                && file.language == Language::Rust
                && file.category == Category::Production
        }));

        std::fs::remove_dir_all(root).unwrap();
    }
}
