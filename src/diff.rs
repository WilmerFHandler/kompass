//! Safe before/after comparison for machine-readable Kompass reports.
//!
//! Reports are deliberately deserialized into private input types here. The
//! analysis model is a serialization contract, and keeping the comparison
//! reader separate means adding output fields does not force every report
//! consumer to depend on the in-memory analysis structs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::identity;
use crate::model::{AnalysisContract, OutputFormat};

const COMPONENT_NAMES: [&str; 8] = [
    "boundary",
    "control decisions",
    "nesting",
    "boolean operators",
    "expression operations",
    "call sites",
    "explicit parameters",
    "match arms",
];

/// Options controlling report comparison.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompareOptions {
    /// Permit the before and after reports to contain different files.
    /// Aggregate deltas then include the burden of added and removed files.
    pub allow_file_changes: bool,
    /// Permit reports captured from equivalent roots such as a worktree and
    /// its checkout. Relative file paths and all other identity checks remain
    /// strict.
    pub allow_root_change: bool,
}

/// Compare two complete JSON reports with the default strict file scope.
pub fn compare_paths(before_path: &Path, after_path: &Path) -> Result<Comparison, DiffError> {
    compare_paths_with_options(before_path, after_path, CompareOptions::default())
}

/// Compare two complete JSON reports using explicit scope options.
pub fn compare_paths_with_options(
    before_path: &Path,
    after_path: &Path,
    options: CompareOptions,
) -> Result<Comparison, DiffError> {
    let before = load_report(before_path)?;
    let after = load_report(after_path)?;
    compare_reports(before_path, after_path, before, after, options)
}

/// Render a completed comparison for a terminal or a machine consumer.
pub fn render_comparison(
    comparison: &Comparison,
    format: OutputFormat,
) -> Result<String, serde_json::Error> {
    match format {
        OutputFormat::Text => Ok(render_text(comparison)),
        OutputFormat::Json => {
            let mut output = serde_json::to_string_pretty(comparison)?;
            output.push('\n');
            Ok(output)
        }
    }
}

/// A report comparison that passed all like-for-like validation checks.
#[derive(Clone, Debug, Serialize)]
pub struct Comparison {
    pub comparable: bool,
    pub before: ReportMetadata,
    pub after: ReportMetadata,
    pub scope: ScopeSummary,
    pub file_changes: FileChanges,
    pub file_burdens: Vec<FileBurdenDelta>,
    /// Burden and score-component deltas partitioned by source language. The
    /// burden and component totals reconcile with the repository deltas.
    pub language_deltas: Vec<LanguageDelta>,
    pub burden: BurdenDelta,
    pub callables: CallableStatsDelta,
    pub macro_opacity: MacroOpacityDelta,
    /// All score components, ordered by descending absolute delta. Text output
    /// shows the first five; JSON keeps the complete fixed component set.
    pub components: Vec<ComponentDelta>,
    pub functions: FunctionChanges,
    pub possible_redistributions: Vec<RedistributionHint>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReportMetadata {
    pub path: String,
    pub root: String,
    pub version: String,
    pub schema_version: String,
    pub evidence_version: String,
    pub model: String,
    pub analysis_contract: AnalysisContract,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScopeSummary {
    pub root: String,
    pub files: usize,
    pub before_files: usize,
    pub after_files: usize,
    pub file_set_changed: bool,
    pub before_test_files: usize,
    pub after_test_files: usize,
    pub coverage_complete: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FileChanges {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileBurdenDelta {
    pub path: String,
    pub production: MetricDelta,
    pub test: MetricDelta,
    pub total: MetricDelta,
}

#[derive(Clone, Debug, Serialize)]
pub struct LanguageDelta {
    pub language: String,
    pub files: MetricDelta,
    pub burden: BurdenDelta,
    pub callables: CallableStatsDelta,
    pub components: Vec<ComponentDelta>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BurdenDelta {
    pub production: MetricDelta,
    pub test: MetricDelta,
    pub total: MetricDelta,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallableStatsDelta {
    pub production: CallableStats,
    pub test: CallableStats,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallableStats {
    pub count: MetricDelta,
    pub p95_score: MetricDelta,
    pub highest_score: MetricDelta,
}

#[derive(Clone, Debug, Serialize)]
pub struct MacroOpacityDelta {
    pub invocations: MetricDelta,
    pub source_tokens: MetricDelta,
    pub definitions: MetricDelta,
    pub definition_tokens: MetricDelta,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricDelta {
    pub before: usize,
    pub after: usize,
    pub delta: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ComponentDelta {
    pub name: String,
    pub before: usize,
    pub after: usize,
    pub delta: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FunctionChanges {
    pub changed: Vec<FunctionChange>,
    pub added: Vec<FunctionChange>,
    pub removed: Vec<FunctionChange>,
    pub moved: Vec<FunctionChange>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Changed,
    Added,
    Removed,
    Moved,
}

#[derive(Clone, Debug, Serialize)]
pub struct FunctionChange {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_name: Option<String>,
    pub name: String,
    pub kind: String,
    pub category: String,
    pub status: ChangeStatus,
    pub before_score: Option<usize>,
    pub after_score: Option<usize>,
    pub delta: i64,
    /// Evidence stage that paired the two units, or `unmatched` for an add or
    /// removal.
    pub match_basis: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RedistributionHint {
    pub path: String,
    pub category: String,
    pub matched_reductions: Vec<FunctionChange>,
    pub added_count: usize,
    pub added_burden: usize,
    pub file_total: MetricDelta,
    pub note: String,
}

#[derive(Debug)]
pub enum DiffError {
    Read { path: PathBuf, message: String },
    Parse { path: PathBuf, message: String },
    Incompatible { issues: Vec<String> },
}

impl fmt::Display for DiffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, message } => {
                write!(
                    formatter,
                    "could not read report {}: {message}",
                    path.display()
                )
            }
            Self::Parse { path, message } => {
                write!(
                    formatter,
                    "could not parse report {}: {message}",
                    path.display()
                )
            }
            Self::Incompatible { issues } => {
                writeln!(formatter, "cannot compare reports safely:")?;
                for issue in issues {
                    writeln!(formatter, "  - {issue}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for DiffError {}

fn load_report(path: &Path) -> Result<InputReport, DiffError> {
    let bytes = fs::read(path).map_err(|error| DiffError::Read {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    serde_json::from_slice(&bytes).map_err(|error| DiffError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn compare_reports(
    before_path: &Path,
    after_path: &Path,
    before: InputReport,
    after: InputReport,
    options: CompareOptions,
) -> Result<Comparison, DiffError> {
    let mut issues = Vec::new();
    validate_identity("before", &before, &mut issues);
    validate_identity("after", &after, &mut issues);

    if before.tool != after.tool {
        issues.push(format!(
            "tool identifiers differ: before is {:?}, after is {:?}",
            before.tool, after.tool
        ));
    }
    if before.model != after.model {
        issues.push(format!(
            "score model differs: before is {:?}, after is {:?}; compare reports generated with the same model",
            before.model, after.model
        ));
    }
    if before.analysis_contract != after.analysis_contract {
        issues.push(format!(
            "analysis contract differs: before is {}, after is {}; compare reports generated with the same frontend and discovery contract",
            before.analysis_contract.display(),
            after.analysis_contract.display()
        ));
    }
    if before.root != after.root && !options.allow_root_change {
        issues.push(format!(
            "analyzed roots differ: before is {:?}, after is {:?}; use the same input root or pass --allow-root-change",
            before.root, after.root
        ));
    }

    validate_coverage("before", &before, &mut issues);
    validate_coverage("after", &after, &mut issues);

    let before_files = file_map("before", &before, &mut issues);
    let after_files = file_map("after", &after, &mut issues);
    let before_paths = before_files.keys().cloned().collect::<BTreeSet<_>>();
    let after_paths = after_files.keys().cloned().collect::<BTreeSet<_>>();
    let file_changes = FileChanges {
        added: after_paths.difference(&before_paths).cloned().collect(),
        removed: before_paths.difference(&after_paths).cloned().collect(),
    };
    if (!file_changes.added.is_empty() || !file_changes.removed.is_empty())
        && !options.allow_file_changes
    {
        let added = &file_changes.added;
        let removed = &file_changes.removed;
        issues.push(format!(
            "file scope differs (added: {}; removed: {}); run both reports over the same source file set or pass --allow-file-changes",
            format_paths(added),
            format_paths(removed)
        ));
    }
    for path in before_paths.intersection(&after_paths) {
        if before_files[path].language != after_files[path].language {
            issues.push(format!(
                "file language differs for {:?}: before is {:?}, after is {:?}",
                path, before_files[path].language, after_files[path].language
            ));
        }
    }

    if !issues.is_empty() {
        return Err(DiffError::Incompatible { issues });
    }

    let mut warnings = Vec::new();
    if !file_changes.added.is_empty() || !file_changes.removed.is_empty() {
        warnings.push(
            "file set changed; aggregate burden and component deltas include added and removed files"
                .to_owned(),
        );
    }
    if before.root != after.root {
        warnings.push(
            "analyzed roots differ; relative file paths are compared after explicit root override"
                .to_owned(),
        );
    }
    if before.version != after.version {
        warnings.push(format!(
            "report versions differ: {} before, {} after; the score model still matches",
            before.version, after.version
        ));
    }
    if before.coverage.test_files != after.coverage.test_files {
        warnings.push(format!(
            "test-file classification changed: {} before, {} after",
            before.coverage.test_files, after.coverage.test_files
        ));
    }

    let before_functions = function_map("before", &before_files)?;
    let after_functions = function_map("after", &after_files)?;
    let (functions, component_before, component_after, mut match_warnings) =
        compare_functions(&before_functions, &after_functions);
    let components = component_deltas(component_before, component_after);
    let file_burdens = file_burden_deltas(&before_files, &after_files);
    let callables = callable_stats_delta(&before_functions, &after_functions);
    let possible_redistributions = possible_redistributions(
        &before_functions,
        &after_functions,
        &before_files,
        &after_files,
    );
    let language_deltas = language_deltas(
        &before_files,
        &after_files,
        &before_functions,
        &after_functions,
    );
    warnings.append(&mut match_warnings);

    Ok(Comparison {
        comparable: true,
        before: metadata(before_path, &before),
        after: metadata(after_path, &after),
        scope: ScopeSummary {
            root: before.root.clone(),
            files: before.files.len(),
            before_files: before.files.len(),
            after_files: after.files.len(),
            file_set_changed: !file_changes.added.is_empty() || !file_changes.removed.is_empty(),
            before_test_files: before.coverage.test_files,
            after_test_files: after.coverage.test_files,
            coverage_complete: before.coverage.complete && after.coverage.complete,
        },
        file_changes,
        file_burdens,
        language_deltas,
        burden: BurdenDelta {
            production: metric_delta(
                before.summary.burden.production,
                after.summary.burden.production,
            ),
            test: metric_delta(before.summary.burden.test, after.summary.burden.test),
            total: metric_delta(before.summary.burden.total, after.summary.burden.total),
        },
        callables,
        macro_opacity: MacroOpacityDelta {
            invocations: metric_delta(
                before.macro_opacity.invocations,
                after.macro_opacity.invocations,
            ),
            source_tokens: metric_delta(
                before.macro_opacity.source_tokens,
                after.macro_opacity.source_tokens,
            ),
            definitions: metric_delta(
                before.macro_opacity.definitions,
                after.macro_opacity.definitions,
            ),
            definition_tokens: metric_delta(
                before.macro_opacity.definition_tokens,
                after.macro_opacity.definition_tokens,
            ),
        },
        components,
        functions,
        possible_redistributions,
        warnings,
    })
}

fn validate_identity(label: &str, report: &InputReport, issues: &mut Vec<String>) {
    if report.tool != "kompass" {
        issues.push(format!(
            "{label} report has tool {:?}; expected \"kompass\"",
            report.tool
        ));
    }
    if report.model.trim().is_empty() {
        issues.push(format!("{label} report has an empty score model"));
    }
    if report.root.trim().is_empty() {
        issues.push(format!("{label} report has an empty analyzed root"));
    }
    if report.schema_version != identity::REPORT_SCHEMA_VERSION {
        issues.push(format!(
            "{label} report has schema {:?}; expected {}; compact or non-analysis reports cannot be used as baselines",
            report.schema_version,
            identity::REPORT_SCHEMA_VERSION
        ));
    }
    if report.evidence_version != identity::EVIDENCE_VERSION {
        issues.push(format!(
            "{label} report has evidence version {:?}; expected {}; regenerate a complete analysis report",
            report.evidence_version,
            identity::EVIDENCE_VERSION
        ));
    }
    if !report.analysis_contract.is_complete() {
        issues.push(format!(
            "{label} report has an incomplete analysis contract; regenerate it with a multi-language Kompass"
        ));
    }
}

fn validate_coverage(label: &str, report: &InputReport, issues: &mut Vec<String>) {
    let coverage = &report.coverage;
    if !coverage.complete {
        issues.push(format!(
            "{label} coverage is incomplete ({} of {} files analyzed)",
            coverage.analyzed_files, coverage.discovered_files
        ));
    }
    if coverage.failed_files > 0 {
        issues.push(format!(
            "{label} report contains {} failed file(s)",
            coverage.failed_files
        ));
    }
    if !report.errors.is_empty() {
        issues.push(format!(
            "{label} report contains {} analysis error(s)",
            report.errors.len()
        ));
    }
    if coverage.discovered_files != coverage.analyzed_files {
        issues.push(format!(
            "{label} coverage counts differ ({} discovered, {} analyzed)",
            coverage.discovered_files, coverage.analyzed_files
        ));
    }
    if coverage.analyzed_files != report.files.len() {
        issues.push(format!(
            "{label} report is internally inconsistent ({} analyzed files, {} file records)",
            coverage.analyzed_files,
            report.files.len()
        ));
    }
}

fn file_map<'a>(
    label: &str,
    report: &'a InputReport,
    issues: &mut Vec<String>,
) -> BTreeMap<String, &'a InputFile> {
    let mut files = BTreeMap::new();
    let mut snapshot_ids = BTreeSet::new();
    for file in &report.files {
        if file.language.trim().is_empty() {
            issues.push(format!(
                "{label} report has no language for file {:?}; regenerate it with a multi-language Kompass",
                file.path
            ));
        }
        if files.insert(file.path.clone(), file).is_some() {
            issues.push(format!(
                "{label} report lists file {:?} more than once",
                file.path
            ));
        }
        for function in &file.functions {
            if function.snapshot_id.trim().is_empty() {
                issues.push(format!(
                    "{label} report has no snapshot id for {:?} in {:?}; compact or legacy unit records cannot be compared",
                    function.name, file.path
                ));
            } else if !snapshot_ids.insert(function.snapshot_id.clone()) {
                issues.push(format!(
                    "{label} report reuses snapshot id {:?}; regenerate the complete analysis report",
                    function.snapshot_id
                ));
            }
            if function.declaration_fingerprint.trim().is_empty()
                || function.body_fingerprint.trim().is_empty()
            {
                issues.push(format!(
                    "{label} report has incomplete lexical evidence for {:?} in {:?}; compact or legacy unit records cannot be compared",
                    function.name, file.path
                ));
            }
        }
    }
    files
}

fn function_map<'a>(
    label: &str,
    files: &BTreeMap<String, &'a InputFile>,
) -> Result<BTreeMap<FunctionKey, &'a InputFunction>, DiffError> {
    let mut functions = BTreeMap::new();
    for (path, file) in files {
        for function in &file.functions {
            let key = FunctionKey {
                path: path.clone(),
                language: file.language.clone(),
                snapshot_id: function.snapshot_id.clone(),
                name: function.name.clone(),
                kind: function.kind.clone(),
                category: function.category.clone(),
            };
            if functions.insert(key.clone(), function).is_some() {
                return Err(DiffError::Incompatible {
                    issues: vec![format!(
                        "{label} report lists function {} more than once in {}",
                        key.display(),
                        path
                    )],
                });
            }
        }
    }
    Ok(functions)
}

/// Analyzer reports identify closures with source locations. Comparisons use
/// their order within each lexical parent instead, so unrelated line shifts do
/// not turn every closure into a removed-and-added pair.
fn compare_functions(
    before: &BTreeMap<FunctionKey, &InputFunction>,
    after: &BTreeMap<FunctionKey, &InputFunction>,
) -> (FunctionChanges, [usize; 8], [usize; 8], Vec<String>) {
    let mut changes = FunctionChanges::default();
    let mut component_before = [0usize; 8];
    let mut component_after = [0usize; 8];

    for before_function in before.values() {
        add_components(&mut component_before, &before_function.score);
    }
    for after_function in after.values() {
        add_components(&mut component_after, &after_function.score);
    }

    let (matches, unmatched_before, unmatched_after, warnings) = staged_matches(before, after);
    for (before_key, after_key, basis) in matches {
        let before_function = before[&before_key];
        let after_function = after[&after_key];
        let moved = before_key.path != after_key.path;
        if moved {
            changes.moved.push(matched_function_change(
                &before_key,
                &after_key,
                before_function,
                after_function,
                ChangeStatus::Moved,
                basis,
            ));
        } else if function_changed(before_function, after_function) {
            changes.changed.push(matched_function_change(
                &before_key,
                &after_key,
                before_function,
                after_function,
                ChangeStatus::Changed,
                basis,
            ));
        }
    }
    for key in unmatched_before {
        let function = before[&key];
        changes.removed.push(function_change(
            &key,
            ChangeStatus::Removed,
            Some(function.score.value),
            None,
        ));
    }
    for key in unmatched_after {
        let function = after[&key];
        changes.added.push(function_change(
            &key,
            ChangeStatus::Added,
            None,
            Some(function.score.value),
        ));
    }

    sort_function_changes(&mut changes.changed);
    sort_function_changes(&mut changes.added);
    sort_function_changes(&mut changes.removed);
    sort_function_changes(&mut changes.moved);
    (changes, component_before, component_after, warnings)
}

type FunctionMatches = (
    Vec<(FunctionKey, FunctionKey, MatchBasis)>,
    Vec<FunctionKey>,
    Vec<FunctionKey>,
    Vec<String>,
);

#[derive(Clone, Copy, Debug)]
enum MatchBasis {
    SnapshotId,
    DeclarationAndBody,
    BodyFingerprint,
    DeclarationFingerprint,
    QualifiedName,
}

impl MatchBasis {
    fn label(self) -> &'static str {
        match self {
            Self::SnapshotId => "snapshot_id",
            Self::DeclarationAndBody => "declaration_and_body",
            Self::BodyFingerprint => "body_fingerprint",
            Self::DeclarationFingerprint => "declaration_fingerprint",
            Self::QualifiedName => "qualified_name",
        }
    }
}

fn staged_matches(
    before: &BTreeMap<FunctionKey, &InputFunction>,
    after: &BTreeMap<FunctionKey, &InputFunction>,
) -> FunctionMatches {
    let mut remaining_before = before.keys().cloned().collect::<BTreeSet<_>>();
    let mut remaining_after = after.keys().cloned().collect::<BTreeSet<_>>();
    let mut matches = Vec::new();
    let mut warnings = Vec::new();

    match_unique_stage(
        MatchBasis::SnapshotId,
        before,
        after,
        &mut remaining_before,
        &mut remaining_after,
        &mut matches,
        &mut warnings,
        |_, function| function.snapshot_id.clone(),
    );
    match_unique_stage(
        MatchBasis::DeclarationAndBody,
        before,
        after,
        &mut remaining_before,
        &mut remaining_after,
        &mut matches,
        &mut warnings,
        |_, function| {
            format!(
                "{}\u{1f}{}",
                function.declaration_fingerprint, function.body_fingerprint
            )
        },
    );
    match_unique_stage(
        MatchBasis::BodyFingerprint,
        before,
        after,
        &mut remaining_before,
        &mut remaining_after,
        &mut matches,
        &mut warnings,
        |_, function| function.body_fingerprint.clone(),
    );
    match_unique_stage(
        MatchBasis::DeclarationFingerprint,
        before,
        after,
        &mut remaining_before,
        &mut remaining_after,
        &mut matches,
        &mut warnings,
        |_, function| function.declaration_fingerprint.clone(),
    );
    match_unique_stage(
        MatchBasis::QualifiedName,
        before,
        after,
        &mut remaining_before,
        &mut remaining_after,
        &mut matches,
        &mut warnings,
        |key, _| key.name.clone(),
    );

    (
        matches,
        remaining_before.into_iter().collect(),
        remaining_after.into_iter().collect(),
        warnings,
    )
}

#[allow(clippy::too_many_arguments)]
fn match_unique_stage<F>(
    basis: MatchBasis,
    before: &BTreeMap<FunctionKey, &InputFunction>,
    after: &BTreeMap<FunctionKey, &InputFunction>,
    remaining_before: &mut BTreeSet<FunctionKey>,
    remaining_after: &mut BTreeSet<FunctionKey>,
    matches: &mut Vec<(FunctionKey, FunctionKey, MatchBasis)>,
    warnings: &mut Vec<String>,
    selector: F,
) where
    F: Fn(&FunctionKey, &InputFunction) -> String,
{
    let mut before_groups = BTreeMap::<String, Vec<FunctionKey>>::new();
    let mut after_groups = BTreeMap::<String, Vec<FunctionKey>>::new();
    for key in remaining_before.iter() {
        let function = before[key];
        before_groups
            .entry(evidence_group_key(key, &selector(key, function)))
            .or_default()
            .push(key.clone());
    }
    for key in remaining_after.iter() {
        let function = after[key];
        after_groups
            .entry(evidence_group_key(key, &selector(key, function)))
            .or_default()
            .push(key.clone());
    }

    let groups = before_groups
        .keys()
        .chain(after_groups.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for group in groups {
        let before_candidates = before_groups.get(&group).cloned().unwrap_or_default();
        let after_candidates = after_groups.get(&group).cloned().unwrap_or_default();
        if before_candidates.len() == 1 && after_candidates.len() == 1 {
            let before_key = before_candidates[0].clone();
            let after_key = after_candidates[0].clone();
            remaining_before.remove(&before_key);
            remaining_after.remove(&after_key);
            matches.push((before_key, after_key, basis));
        } else if !before_candidates.is_empty()
            && !after_candidates.is_empty()
            && (before_candidates.len() > 1 || after_candidates.len() > 1)
        {
            warnings.push(format!(
                "ambiguous {} match for {} before unit(s) and {} after unit(s); units remain unmatched",
                basis.label(),
                before_candidates.len(),
                after_candidates.len()
            ));
        }
    }
}

fn evidence_group_key(key: &FunctionKey, evidence: &str) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        key.language, key.kind, key.category, evidence
    )
}

fn file_burden_deltas(
    before: &BTreeMap<String, &InputFile>,
    after: &BTreeMap<String, &InputFile>,
) -> Vec<FileBurdenDelta> {
    let paths = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    paths
        .into_iter()
        .map(|path| {
            let before_file = before.get(&path);
            let after_file = after.get(&path);
            let before_burden = before_file.map(|file| &file.burden);
            let after_burden = after_file.map(|file| &file.burden);
            FileBurdenDelta {
                path,
                production: metric_delta(
                    before_burden.map_or(0, |burden| burden.production),
                    after_burden.map_or(0, |burden| burden.production),
                ),
                test: metric_delta(
                    before_burden.map_or(0, |burden| burden.test),
                    after_burden.map_or(0, |burden| burden.test),
                ),
                total: metric_delta(
                    before_burden.map_or(0, |burden| burden.total),
                    after_burden.map_or(0, |burden| burden.total),
                ),
            }
        })
        .collect()
}

fn callable_stats_delta(
    before: &BTreeMap<FunctionKey, &InputFunction>,
    after: &BTreeMap<FunctionKey, &InputFunction>,
) -> CallableStatsDelta {
    CallableStatsDelta {
        production: callable_stats(before, after, "production"),
        test: callable_stats(before, after, "test"),
    }
}

fn callable_stats(
    before: &BTreeMap<FunctionKey, &InputFunction>,
    after: &BTreeMap<FunctionKey, &InputFunction>,
    category: &str,
) -> CallableStats {
    let before_scores = scores_for_category(before, category);
    let after_scores = scores_for_category(after, category);
    CallableStats {
        count: metric_delta(before_scores.len(), after_scores.len()),
        p95_score: metric_delta(p95_score(&before_scores), p95_score(&after_scores)),
        highest_score: metric_delta(
            before_scores.iter().copied().max().unwrap_or(0),
            after_scores.iter().copied().max().unwrap_or(0),
        ),
    }
}

fn language_deltas(
    before_files: &BTreeMap<String, &InputFile>,
    after_files: &BTreeMap<String, &InputFile>,
    before_functions: &BTreeMap<FunctionKey, &InputFunction>,
    after_functions: &BTreeMap<FunctionKey, &InputFunction>,
) -> Vec<LanguageDelta> {
    let languages = before_files
        .values()
        .chain(after_files.values())
        .map(|file| file.language.clone())
        .collect::<BTreeSet<_>>();
    languages
        .into_iter()
        .map(|language| {
            let before_language_functions = functions_for_language(before_functions, &language);
            let after_language_functions = functions_for_language(after_functions, &language);
            let mut component_before = [0usize; 8];
            let mut component_after = [0usize; 8];
            for function in before_language_functions.values() {
                add_components(&mut component_before, &function.score);
            }
            for function in after_language_functions.values() {
                add_components(&mut component_after, &function.score);
            }
            let before_burden = file_burden_for_language(before_files, &language);
            let after_burden = file_burden_for_language(after_files, &language);
            LanguageDelta {
                language: language.clone(),
                files: metric_delta(
                    before_files
                        .values()
                        .filter(|file| file.language == language)
                        .count(),
                    after_files
                        .values()
                        .filter(|file| file.language == language)
                        .count(),
                ),
                burden: BurdenDelta {
                    production: metric_delta(before_burden.production, after_burden.production),
                    test: metric_delta(before_burden.test, after_burden.test),
                    total: metric_delta(before_burden.total, after_burden.total),
                },
                callables: callable_stats_delta(
                    &before_language_functions,
                    &after_language_functions,
                ),
                components: component_deltas(component_before, component_after),
            }
        })
        .collect()
}

fn functions_for_language<'a>(
    functions: &BTreeMap<FunctionKey, &'a InputFunction>,
    language: &str,
) -> BTreeMap<FunctionKey, &'a InputFunction> {
    functions
        .iter()
        .filter(|(key, _)| key.language == language)
        .map(|(key, function)| (key.clone(), *function))
        .collect()
}

fn file_burden_for_language(files: &BTreeMap<String, &InputFile>, language: &str) -> InputBurden {
    let mut burden = InputBurden {
        production: 0,
        test: 0,
        total: 0,
    };
    for file in files.values().filter(|file| file.language == language) {
        burden.production = burden.production.saturating_add(file.burden.production);
        burden.test = burden.test.saturating_add(file.burden.test);
        burden.total = burden.total.saturating_add(file.burden.total);
    }
    burden
}

fn scores_for_category(
    functions: &BTreeMap<FunctionKey, &InputFunction>,
    category: &str,
) -> Vec<usize> {
    functions
        .values()
        .filter(|function| function.category == category)
        .map(|function| function.score.value)
        .collect()
}

fn p95_score(scores: &[usize]) -> usize {
    if scores.is_empty() {
        return 0;
    }
    let mut sorted = scores.to_vec();
    sorted.sort_unstable();
    let rank = sorted.len().saturating_mul(19).saturating_add(19) / 20;
    sorted[rank.saturating_sub(1)]
}

fn possible_redistributions<'before, 'after>(
    before_functions: &BTreeMap<FunctionKey, &'before InputFunction>,
    after_functions: &BTreeMap<FunctionKey, &'after InputFunction>,
    before_files: &BTreeMap<String, &'before InputFile>,
    after_files: &BTreeMap<String, &'after InputFile>,
) -> Vec<RedistributionHint> {
    let mut hints = Vec::new();
    for path in before_files
        .keys()
        .filter(|path| after_files.contains_key(*path))
    {
        for category in ["production", "test"] {
            if let Some(hint) = redistribution_hint(
                path,
                category,
                before_functions,
                after_functions,
                before_files[path],
                after_files[path],
            ) {
                hints.push(hint);
            }
        }
    }
    hints.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.category.cmp(&right.category))
    });
    hints
}

fn redistribution_hint(
    path: &str,
    category: &str,
    before_functions: &BTreeMap<FunctionKey, &InputFunction>,
    after_functions: &BTreeMap<FunctionKey, &InputFunction>,
    before_file: &InputFile,
    after_file: &InputFile,
) -> Option<RedistributionHint> {
    let mut matched_reductions =
        collect_matched_reductions(path, category, before_functions, after_functions);
    let (added_count, added_burden) =
        added_callable_summary(path, category, before_functions, after_functions);
    let before_total = category_file_burden(before_file, category);
    let after_total = category_file_burden(after_file, category);
    if matched_reductions.is_empty() || added_count == 0 || after_total < before_total {
        return None;
    }

    sort_function_changes(&mut matched_reductions);
    Some(RedistributionHint {
        path: path.to_owned(),
        category: category.to_owned(),
        matched_reductions,
        added_count,
        added_burden,
        file_total: metric_delta(before_total, after_total),
        note:
            "This is only a possible redistribution; it proves no call or extraction relationship."
                .to_owned(),
    })
}

fn collect_matched_reductions(
    path: &str,
    category: &str,
    before_functions: &BTreeMap<FunctionKey, &InputFunction>,
    after_functions: &BTreeMap<FunctionKey, &InputFunction>,
) -> Vec<FunctionChange> {
    let mut reductions = Vec::new();
    for (key, before_function) in before_functions {
        if key.path != path || key.category != category {
            continue;
        }
        if let Some(after_function) = after_functions.get(key)
            && after_function.score.value < before_function.score.value
        {
            reductions.push(function_change(
                key,
                ChangeStatus::Changed,
                Some(before_function.score.value),
                Some(after_function.score.value),
            ));
        }
    }
    reductions
}

fn added_callable_summary(
    path: &str,
    category: &str,
    before_functions: &BTreeMap<FunctionKey, &InputFunction>,
    after_functions: &BTreeMap<FunctionKey, &InputFunction>,
) -> (usize, usize) {
    let mut count = 0usize;
    let mut burden = 0usize;
    for (key, after_function) in after_functions {
        if key.path == path && key.category == category && !before_functions.contains_key(key) {
            count = count.saturating_add(1);
            burden = burden.saturating_add(after_function.score.value);
        }
    }
    (count, burden)
}

fn category_file_burden(file: &InputFile, category: &str) -> usize {
    if category == "production" {
        file.burden.production
    } else {
        file.burden.test
    }
}

fn function_changed(before: &InputFunction, after: &InputFunction) -> bool {
    before.score != after.score
        || before.metrics != after.metrics
        || canonical_unit_name(&before.name) != canonical_unit_name(&after.name)
        || before.kind != after.kind
        || before.category != after.category
        || before.declaration_fingerprint != after.declaration_fingerprint
        || before.body_fingerprint != after.body_fingerprint
}

fn canonical_unit_name(name: &str) -> String {
    name.split("::")
        .map(|segment| {
            if (segment.starts_with("<closure@") || segment.starts_with("<lambda@"))
                && segment.ends_with('>')
            {
                let kind = if segment.starts_with("<closure@") {
                    "closure"
                } else {
                    "lambda"
                };
                format!("<{kind}>")
            } else {
                segment.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("::")
}

fn function_change(
    key: &FunctionKey,
    status: ChangeStatus,
    before_score: Option<usize>,
    after_score: Option<usize>,
) -> FunctionChange {
    FunctionChange {
        path: key.path.clone(),
        before_path: matches!(&status, ChangeStatus::Removed).then_some(key.path.clone()),
        after_path: matches!(&status, ChangeStatus::Added).then_some(key.path.clone()),
        before_name: matches!(&status, ChangeStatus::Removed).then_some(key.name.clone()),
        after_name: matches!(&status, ChangeStatus::Added).then_some(key.name.clone()),
        name: key.name.clone(),
        kind: key.kind.clone(),
        category: key.category.clone(),
        status,
        before_score,
        after_score,
        delta: optional_delta(before_score, after_score),
        match_basis: "unmatched".to_owned(),
    }
}

fn matched_function_change(
    before_key: &FunctionKey,
    after_key: &FunctionKey,
    before: &InputFunction,
    after: &InputFunction,
    status: ChangeStatus,
    basis: MatchBasis,
) -> FunctionChange {
    FunctionChange {
        path: after_key.path.clone(),
        before_path: Some(before_key.path.clone()),
        after_path: Some(after_key.path.clone()),
        before_name: Some(before.name.clone()),
        after_name: Some(after.name.clone()),
        name: after.name.clone(),
        kind: after.kind.clone(),
        category: after.category.clone(),
        status,
        before_score: Some(before.score.value),
        after_score: Some(after.score.value),
        delta: optional_delta(Some(before.score.value), Some(after.score.value)),
        match_basis: basis.label().to_owned(),
    }
}

fn sort_function_changes(changes: &mut [FunctionChange]) {
    changes.sort_by(|left, right| {
        right
            .delta
            .unsigned_abs()
            .cmp(&left.delta.unsigned_abs())
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.kind.cmp(&right.kind))
    });
}

fn add_components(total: &mut [usize; 8], score: &InputScore) {
    let values = score_components(score);
    for (slot, value) in total.iter_mut().zip(values) {
        *slot = slot.saturating_add(value);
    }
}

fn component_deltas(before: [usize; 8], after: [usize; 8]) -> Vec<ComponentDelta> {
    let mut components = COMPONENT_NAMES
        .into_iter()
        .enumerate()
        .map(|(index, name)| ComponentDelta {
            name: name.to_owned(),
            before: before[index],
            after: after[index],
            delta: signed_delta(before[index], after[index]),
        })
        .collect::<Vec<_>>();
    components.sort_by(|left, right| {
        right
            .delta
            .unsigned_abs()
            .cmp(&left.delta.unsigned_abs())
            .then_with(|| left.name.cmp(&right.name))
    });
    components
}

fn score_components(score: &InputScore) -> [usize; 8] {
    [
        score.boundary,
        score.control_decisions.saturating_mul(10),
        score.nesting_penalty.saturating_mul(10),
        score.boolean_operator_units,
        score.expression_operation_units,
        score.call_site_units,
        score.parameter_units,
        score.match_arm_units,
    ]
}

fn metadata(path: &Path, report: &InputReport) -> ReportMetadata {
    ReportMetadata {
        path: path.to_string_lossy().into_owned(),
        root: report.root.clone(),
        version: report.version.clone(),
        schema_version: report.schema_version.clone(),
        evidence_version: report.evidence_version.clone(),
        model: report.model.clone(),
        analysis_contract: report.analysis_contract.clone(),
    }
}

fn metric_delta(before: usize, after: usize) -> MetricDelta {
    MetricDelta {
        before,
        after,
        delta: signed_delta(before, after),
    }
}

fn optional_delta(before: Option<usize>, after: Option<usize>) -> i64 {
    signed_delta(before.unwrap_or(0), after.unwrap_or(0))
}

fn signed_delta(before: usize, after: usize) -> i64 {
    if after >= before {
        after.saturating_sub(before).try_into().unwrap_or(i64::MAX)
    } else {
        before
            .saturating_sub(after)
            .try_into()
            .map(|value: i64| value.saturating_neg())
            .unwrap_or(i64::MIN)
    }
}

fn format_paths(paths: &[String]) -> String {
    if paths.is_empty() {
        "none".to_owned()
    } else {
        paths.join(", ")
    }
}

fn render_text(comparison: &Comparison) -> String {
    let mut output = String::new();
    use std::fmt::Write as _;

    writeln!(output, "Kompass diff · {}", comparison.scope.root).unwrap();
    writeln!(
        output,
        "Before: {} · {} · {}",
        comparison.before.path, comparison.before.version, comparison.before.model
    )
    .unwrap();
    writeln!(
        output,
        "After:  {} · {} · {}",
        comparison.after.path, comparison.after.version, comparison.after.model
    )
    .unwrap();
    writeln!(
        output,
        "Analysis contract · {}",
        comparison.before.analysis_contract.display()
    )
    .unwrap();
    writeln!(
        output,
        "Evidence · schema {} · {}",
        comparison.before.schema_version, comparison.before.evidence_version
    )
    .unwrap();
    if comparison.scope.file_set_changed {
        writeln!(
            output,
            "WARNING: file set changed · {} files before → {} files after",
            comparison.scope.before_files, comparison.scope.after_files
        )
        .unwrap();
        render_file_changes(&mut output, &comparison.file_changes);
        writeln!(
            output,
            "Warning: aggregate burden and component deltas include added and removed files."
        )
        .unwrap();
    } else {
        writeln!(
            output,
            "Scope comparable · {} files · complete coverage",
            comparison.scope.files
        )
        .unwrap();
    }

    output.push('\n');
    render_metric(
        &mut output,
        "Production burden",
        &comparison.burden.production,
        true,
    );
    render_metric(&mut output, "Test burden", &comparison.burden.test, true);
    render_metric(
        &mut output,
        "Repository burden",
        &comparison.burden.total,
        true,
    );

    output.push('\n');
    writeln!(output, "Callable summary deltas").unwrap();
    render_callable_stats(&mut output, "Production", &comparison.callables.production);
    render_callable_stats(&mut output, "Tests", &comparison.callables.test);
    render_language_deltas(&mut output, &comparison.language_deltas);

    render_file_burdens(&mut output, &comparison.file_burdens);

    output.push('\n');
    writeln!(output, "Callables").unwrap();
    render_function_group(
        &mut output,
        "Changed callables",
        &comparison.functions.changed,
    );
    render_function_group(&mut output, "Added callables", &comparison.functions.added);
    render_function_group(
        &mut output,
        "Removed callables",
        &comparison.functions.removed,
    );
    render_function_group(&mut output, "Moved callables", &comparison.functions.moved);

    if !comparison.possible_redistributions.is_empty() {
        output.push('\n');
        writeln!(output, "Possible redistribution").unwrap();
        for hint in &comparison.possible_redistributions {
            render_redistribution(&mut output, hint);
        }
    }

    output.push('\n');
    writeln!(output, "Top score component deltas").unwrap();
    for component in comparison
        .components
        .iter()
        .filter(|component| component.delta != 0)
        .take(5)
    {
        writeln!(
            output,
            "  {:+6.1}  {} ({} → {})",
            component.delta as f64 / 10.0,
            component.name,
            format_score(component.before),
            format_score(component.after)
        )
        .unwrap();
    }
    if comparison
        .components
        .iter()
        .all(|component| component.delta == 0)
    {
        writeln!(output, "  No score component changed.").unwrap();
    }

    output.push('\n');
    writeln!(
        output,
        "Macro opacity · invocations {} → {} ({:+}) · source tokens {} → {} ({:+})",
        comparison.macro_opacity.invocations.before,
        comparison.macro_opacity.invocations.after,
        comparison.macro_opacity.invocations.delta,
        comparison.macro_opacity.source_tokens.before,
        comparison.macro_opacity.source_tokens.after,
        comparison.macro_opacity.source_tokens.delta,
    )
    .unwrap();
    writeln!(
        output,
        "Macro definitions · {} → {} ({:+}) · definition tokens {} → {} ({:+})",
        comparison.macro_opacity.definitions.before,
        comparison.macro_opacity.definitions.after,
        comparison.macro_opacity.definitions.delta,
        comparison.macro_opacity.definition_tokens.before,
        comparison.macro_opacity.definition_tokens.after,
        comparison.macro_opacity.definition_tokens.delta,
    )
    .unwrap();
    if comparison.scope.before_test_files != comparison.scope.after_test_files {
        writeln!(
            output,
            "Test-file classification · {} → {}",
            comparison.scope.before_test_files, comparison.scope.after_test_files
        )
        .unwrap();
    }

    if !comparison.warnings.is_empty() {
        output.push('\n');
        writeln!(output, "Warnings").unwrap();
        for warning in &comparison.warnings {
            writeln!(output, "  - {warning}").unwrap();
        }
    }
    output
}

fn render_file_changes(output: &mut String, changes: &FileChanges) {
    use std::fmt::Write as _;
    writeln!(output, "Added files · {}", changes.added.len()).unwrap();
    for path in &changes.added {
        writeln!(output, "  + {path}").unwrap();
    }
    writeln!(output, "Removed files · {}", changes.removed.len()).unwrap();
    for path in &changes.removed {
        writeln!(output, "  - {path}").unwrap();
    }
}

fn render_file_burdens(output: &mut String, file_burdens: &[FileBurdenDelta]) {
    use std::fmt::Write as _;
    let changed = file_burdens
        .iter()
        .filter(|file| file.total.delta != 0 || file.production.delta != 0 || file.test.delta != 0)
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return;
    }
    output.push('\n');
    writeln!(output, "File burden deltas · {} changed", changed.len()).unwrap();
    for file in changed {
        writeln!(
            output,
            "  {} · total {:+.1} · production {:+.1} · tests {:+.1}",
            file.path,
            file.total.delta as f64 / 10.0,
            file.production.delta as f64 / 10.0,
            file.test.delta as f64 / 10.0,
        )
        .unwrap();
    }
}

fn render_callable_stats(output: &mut String, label: &str, stats: &CallableStats) {
    use std::fmt::Write as _;
    writeln!(
        output,
        "{label} · count {} → {} ({:+}) · p95 {} → {} ({:+.1}) · highest {} → {} ({:+.1})",
        stats.count.before,
        stats.count.after,
        stats.count.delta,
        format_score(stats.p95_score.before),
        format_score(stats.p95_score.after),
        stats.p95_score.delta as f64 / 10.0,
        format_score(stats.highest_score.before),
        format_score(stats.highest_score.after),
        stats.highest_score.delta as f64 / 10.0,
    )
    .unwrap();
}

fn render_language_deltas(output: &mut String, deltas: &[LanguageDelta]) {
    use std::fmt::Write as _;
    writeln!(output, "Language deltas").unwrap();
    for delta in deltas {
        writeln!(
            output,
            "{} · files {} → {} ({:+}) · burden {} → {} ({:+.1})",
            delta.language,
            delta.files.before,
            delta.files.after,
            delta.files.delta,
            format_score(delta.burden.total.before),
            format_score(delta.burden.total.after),
            delta.burden.total.delta as f64 / 10.0,
        )
        .unwrap();
    }
}

fn render_redistribution(output: &mut String, hint: &RedistributionHint) {
    use std::fmt::Write as _;
    writeln!(
        output,
        "  {} [{}] · matched reductions {} · added burden {} · file total {:+.1}",
        hint.path,
        hint.category,
        hint.matched_reductions.len(),
        format_score(hint.added_burden),
        hint.file_total.delta as f64 / 10.0,
    )
    .unwrap();
    for reduction in &hint.matched_reductions {
        writeln!(
            output,
            "       reduced {} ({:+.1})",
            reduction.name,
            reduction.delta as f64 / 10.0
        )
        .unwrap();
    }
    writeln!(output, "       {}", hint.note).unwrap();
}

fn render_metric(output: &mut String, label: &str, metric: &MetricDelta, score: bool) {
    use std::fmt::Write as _;
    if score {
        writeln!(
            output,
            "{label} · {} → {} ({:+.1})",
            format_score(metric.before),
            format_score(metric.after),
            metric.delta as f64 / 10.0
        )
        .unwrap();
    } else {
        writeln!(
            output,
            "{label} · {} → {} ({:+})",
            metric.before, metric.after, metric.delta
        )
        .unwrap();
    }
}

fn render_function_group(output: &mut String, label: &str, changes: &[FunctionChange]) {
    use std::fmt::Write as _;
    writeln!(output, "{label} · {}", changes.len()).unwrap();
    for change in changes {
        let before = change
            .before_score
            .map(format_score)
            .unwrap_or_else(|| "—".to_owned());
        let after = change
            .after_score
            .map(format_score)
            .unwrap_or_else(|| "—".to_owned());
        if let (Some(before_path), Some(after_path)) = (&change.before_path, &change.after_path)
            && before_path != after_path
        {
            writeln!(
                output,
                "  {:+6.1}  {} → {} · {} [{}; {}] ({} → {})",
                change.delta as f64 / 10.0,
                before_path,
                after_path,
                change.name,
                change.category,
                change.match_basis,
                before,
                after
            )
            .unwrap();
        } else {
            writeln!(
                output,
                "  {:+6.1}  {} · {} [{}; {}] ({} → {})",
                change.delta as f64 / 10.0,
                change.path,
                change.name,
                change.category,
                change.match_basis,
                before,
                after
            )
            .unwrap();
        }
    }
}

fn format_score(units: usize) -> String {
    format!("{:.1}", units as f64 / 10.0)
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FunctionKey {
    path: String,
    language: String,
    snapshot_id: String,
    name: String,
    kind: String,
    category: String,
}

impl FunctionKey {
    fn display(&self) -> String {
        format!("{}::{} [{}]", self.path, self.name, self.category)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct InputReport {
    tool: String,
    version: String,
    #[serde(default)]
    schema_version: String,
    #[serde(default)]
    evidence_version: String,
    model: String,
    #[serde(default)]
    analysis_contract: AnalysisContract,
    root: String,
    summary: InputSummary,
    coverage: InputCoverage,
    macro_opacity: InputMacroOpacity,
    files: Vec<InputFile>,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct InputSummary {
    burden: InputBurden,
}

#[derive(Clone, Debug, Deserialize)]
struct InputBurden {
    production: usize,
    test: usize,
    total: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct InputCoverage {
    discovered_files: usize,
    analyzed_files: usize,
    failed_files: usize,
    test_files: usize,
    complete: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct InputMacroOpacity {
    invocations: usize,
    source_tokens: usize,
    #[serde(default)]
    definitions: usize,
    #[serde(default)]
    definition_tokens: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct InputFile {
    path: String,
    #[serde(default)]
    language: String,
    functions: Vec<InputFunction>,
    #[allow(dead_code)]
    burden: InputBurden,
    #[allow(dead_code)]
    macro_opacity: InputMacroOpacity,
}

#[derive(Clone, Debug, Deserialize)]
struct InputFunction {
    #[serde(default)]
    snapshot_id: String,
    #[serde(default)]
    declaration_fingerprint: String,
    #[serde(default)]
    body_fingerprint: String,
    name: String,
    kind: String,
    category: String,
    metrics: InputMetrics,
    score: InputScore,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
struct InputMetrics {
    #[serde(default)]
    code_lines: usize,
    #[serde(default)]
    statements: usize,
    #[serde(default)]
    expression_operations: usize,
    #[serde(default)]
    decisions: usize,
    #[serde(default)]
    control_decisions: usize,
    #[serde(default)]
    nesting_penalty: usize,
    #[serde(default)]
    max_depth: usize,
    #[serde(default)]
    macro_calls: usize,
    #[serde(default)]
    branches: usize,
    #[serde(default)]
    boolean_operators: usize,
    #[serde(default)]
    match_arms: usize,
    #[serde(default)]
    loops: usize,
    #[serde(default)]
    returns: usize,
    #[serde(default)]
    mutations: usize,
    #[serde(default)]
    parameters: usize,
    #[serde(default)]
    explicit_parameters: usize,
    #[serde(default)]
    call_sites: usize,
    #[serde(default)]
    closures: usize,
    #[serde(default)]
    unsafe_blocks: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
struct InputScore {
    value: usize,
    #[serde(default)]
    control_decisions: usize,
    #[serde(default)]
    nesting_penalty: usize,
    #[serde(default)]
    boundary: usize,
    #[serde(default)]
    boolean_operator_units: usize,
    #[serde(default)]
    call_site_units: usize,
    #[serde(default)]
    parameter_units: usize,
    #[serde(default)]
    match_arm_units: usize,
    #[serde(default)]
    expression_operation_units: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(
        root: &str,
        production: usize,
        functions: Vec<InputFile>,
        invocations: usize,
        source_tokens: usize,
    ) -> InputReport {
        InputReport {
            tool: "kompass".to_owned(),
            version: "0.1.0".to_owned(),
            schema_version: identity::REPORT_SCHEMA_VERSION.to_owned(),
            evidence_version: identity::EVIDENCE_VERSION.to_owned(),
            model: "test-model".to_owned(),
            analysis_contract: AnalysisContract {
                version: "analysis-v1".to_owned(),
                frontend: "frontends-v1".to_owned(),
                discovery: "multi-language-v1".to_owned(),
            },
            root: root.to_owned(),
            summary: InputSummary {
                burden: InputBurden {
                    production,
                    test: 0,
                    total: production,
                },
            },
            coverage: InputCoverage {
                discovered_files: functions.len(),
                analyzed_files: functions.len(),
                failed_files: 0,
                test_files: 0,
                complete: true,
            },
            macro_opacity: InputMacroOpacity {
                invocations,
                source_tokens,
                definitions: 0,
                definition_tokens: 0,
            },
            files: functions,
            errors: Vec::new(),
        }
    }

    fn file(path: &str, functions: Vec<InputFunction>) -> InputFile {
        file_with_language(path, "rust", functions)
    }

    fn file_with_language(path: &str, language: &str, functions: Vec<InputFunction>) -> InputFile {
        InputFile {
            path: path.to_owned(),
            language: language.to_owned(),
            burden: InputBurden {
                production: functions.iter().map(|f| f.score.value).sum(),
                test: 0,
                total: functions.iter().map(|f| f.score.value).sum(),
            },
            macro_opacity: InputMacroOpacity {
                invocations: 0,
                source_tokens: 0,
                definitions: 0,
                definition_tokens: 0,
            },
            functions,
        }
    }

    fn function(name: &str, value: usize, operations: usize) -> InputFunction {
        InputFunction {
            snapshot_id: format!("snapshot-{name}-{operations}"),
            declaration_fingerprint: format!("declaration-{name}-{operations}"),
            body_fingerprint: format!("body-{name}-{operations}"),
            name: name.to_owned(),
            kind: "function".to_owned(),
            category: "production".to_owned(),
            metrics: InputMetrics {
                expression_operations: operations,
                ..InputMetrics::default()
            },
            score: InputScore {
                value,
                expression_operation_units: operations,
                boundary: 10,
                ..InputScore::default()
            },
        }
    }

    fn closure(name: &str, value: usize) -> InputFunction {
        let mut function = function(name, value, 0);
        function.kind = "closure".to_owned();
        function.snapshot_id = format!("closure-{name}");
        function.declaration_fingerprint = "closure-declaration".to_owned();
        function.body_fingerprint = "closure-body".to_owned();
        function
    }

    fn lambda(name: &str, value: usize) -> InputFunction {
        let mut function = function(name, value, 0);
        function.kind = "lambda".to_owned();
        function.snapshot_id = format!("lambda-{name}");
        function.declaration_fingerprint = "lambda-declaration".to_owned();
        function.body_fingerprint = "lambda-body".to_owned();
        function
    }

    #[test]
    fn comparison_keeps_added_removed_and_changed_functions_separate() {
        let before = report(
            "/repo",
            20,
            vec![file(
                "src/lib.rs",
                vec![function("stable", 10, 0), function("removed", 10, 0)],
            )],
            2,
            12,
        );
        let after = report(
            "/repo",
            24,
            vec![file(
                "src/lib.rs",
                vec![function("stable", 14, 4), function("added", 10, 0)],
            )],
            1,
            8,
        );

        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap();

        assert_eq!(comparison.functions.changed.len(), 1);
        assert_eq!(comparison.functions.added.len(), 1);
        assert_eq!(comparison.functions.removed.len(), 1);
        assert_eq!(comparison.functions.changed[0].delta, 4);
        assert_eq!(comparison.burden.production.delta, 4);
        assert_eq!(comparison.macro_opacity.invocations.delta, -1);
        assert_eq!(comparison.macro_opacity.source_tokens.delta, -4);
    }

    #[test]
    fn comparison_matches_closures_across_unrelated_line_shifts() {
        let before = report(
            "/repo",
            10,
            vec![file("src/lib.rs", vec![closure("run::<closure@10:5>", 10)])],
            0,
            0,
        );
        let after = report(
            "/repo",
            10,
            vec![file("src/lib.rs", vec![closure("run::<closure@30:9>", 10)])],
            0,
            0,
        );

        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap();

        assert!(comparison.functions.changed.is_empty());
        assert!(comparison.functions.added.is_empty());
        assert!(comparison.functions.removed.is_empty());
    }

    #[test]
    fn comparison_matches_lambdas_across_unrelated_line_shifts() {
        let before = report(
            "/repo",
            10,
            vec![file("module.py", vec![lambda("run::<lambda@10:5>", 10)])],
            0,
            0,
        );
        let after = report(
            "/repo",
            10,
            vec![file("module.py", vec![lambda("run::<lambda@30:9>", 10)])],
            0,
            0,
        );

        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap();

        assert!(comparison.functions.changed.is_empty());
        assert!(comparison.functions.added.is_empty());
        assert!(comparison.functions.removed.is_empty());
    }

    #[test]
    fn rejects_model_root_and_incomplete_coverage_mismatches() {
        let mut before = report("/before", 0, vec![file("src/lib.rs", vec![])], 0, 0);
        let mut after = report("/after", 0, vec![file("src/lib.rs", vec![])], 0, 0);
        before.coverage.complete = false;
        after.model = "other-model".to_owned();

        let error = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("score model differs"));
        assert!(message.contains("analyzed roots differ"));
        assert!(message.contains("before coverage is incomplete"));
    }

    #[test]
    fn sorts_components_by_absolute_delta() {
        let mut before = [0usize; 8];
        let mut after = [0usize; 8];
        before[0] = 10;
        after[0] = 30;
        before[1] = 20;
        after[1] = 0;
        let components = component_deltas(before, after);
        assert_eq!(components[0].name, "boundary");
        assert_eq!(components[0].delta, 20);
        assert_eq!(components[1].name, "control decisions");
        assert_eq!(components[1].delta, -20);
    }

    #[test]
    fn reports_callable_stats_file_burdens_and_possible_redistribution() {
        let before = report(
            "/repo",
            30,
            vec![file(
                "src/lib.rs",
                vec![function("reduced", 20, 0), function("stable", 10, 0)],
            )],
            0,
            0,
        );
        let after = report(
            "/repo",
            30,
            vec![file(
                "src/lib.rs",
                vec![function("reduced", 10, 0), function("added", 20, 0)],
            )],
            0,
            0,
        );

        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap();

        assert_eq!(comparison.callables.production.count.before, 2);
        assert_eq!(comparison.callables.production.count.after, 2);
        assert_eq!(comparison.callables.production.highest_score.delta, 0);
        assert_eq!(comparison.file_burdens.len(), 1);
        assert_eq!(comparison.file_burdens[0].total.delta, 0);
        assert_eq!(comparison.possible_redistributions.len(), 1);
        let hint = &comparison.possible_redistributions[0];
        assert_eq!(hint.matched_reductions.len(), 1);
        assert_eq!(hint.added_burden, 20);
        assert_eq!(hint.file_total.delta, 0);
        assert!(hint.note.contains("no call or extraction relationship"));
    }

    #[test]
    fn allow_file_changes_keeps_scope_lists_and_warning() {
        let before = report(
            "/repo",
            10,
            vec![file("src/lib.rs", vec![function("old", 10, 0)])],
            0,
            0,
        );
        let after = report(
            "/repo",
            20,
            vec![
                file("src/lib.rs", vec![function("old", 10, 0)]),
                file("src/new.rs", vec![function("new", 10, 0)]),
            ],
            0,
            0,
        );

        let error = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("--allow-file-changes"));

        let before = report(
            "/repo",
            10,
            vec![file("src/lib.rs", vec![function("old", 10, 0)])],
            0,
            0,
        );
        let after = report(
            "/repo",
            20,
            vec![
                file("src/lib.rs", vec![function("old", 10, 0)]),
                file("src/new.rs", vec![function("new", 10, 0)]),
            ],
            0,
            0,
        );
        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions {
                allow_file_changes: true,
                allow_root_change: false,
            },
        )
        .unwrap();
        assert_eq!(comparison.file_changes.added, vec!["src/new.rs"]);
        assert!(comparison.scope.file_set_changed);
        assert!(
            comparison
                .warnings
                .iter()
                .any(|warning| warning.contains("file set changed"))
        );
    }

    #[test]
    fn allows_root_change_only_with_explicit_option() {
        let before = report(
            "/checkout",
            10,
            vec![file("src/lib.rs", vec![function("run", 10, 0)])],
            0,
            0,
        );
        let after = report(
            "/worktree",
            10,
            vec![file("src/lib.rs", vec![function("run", 10, 0)])],
            0,
            0,
        );
        let error = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before.clone(),
            after.clone(),
            CompareOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("--allow-root-change"));

        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions {
                allow_root_change: true,
                ..CompareOptions::default()
            },
        )
        .unwrap();
        assert!(
            comparison
                .warnings
                .iter()
                .any(|warning| warning.contains("roots differ"))
        );
    }

    #[test]
    fn accepts_duplicate_legitimate_names_when_evidence_is_unique() {
        let before = report(
            "/repo",
            20,
            vec![file(
                "src/lib.rs",
                vec![function("property", 10, 1), function("property", 10, 2)],
            )],
            0,
            0,
        );
        let after = report(
            "/repo",
            20,
            vec![file(
                "src/lib.rs",
                vec![function("property", 10, 1), function("property", 10, 2)],
            )],
            0,
            0,
        );
        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap();
        assert!(comparison.functions.changed.is_empty());
        assert!(comparison.functions.added.is_empty());
        assert!(comparison.functions.removed.is_empty());
    }

    #[test]
    fn reports_unique_move_and_rename_with_body_evidence() {
        let mut moved = function("renamed", 10, 0);
        moved.body_fingerprint = "shared-body".to_owned();
        let mut before_function = function("original", 10, 0);
        before_function.body_fingerprint = "shared-body".to_owned();
        let before = report(
            "/repo",
            10,
            vec![file("src/old.rs", vec![before_function])],
            0,
            0,
        );
        let after = report("/repo", 10, vec![file("src/new.rs", vec![moved])], 0, 0);
        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions {
                allow_file_changes: true,
                ..CompareOptions::default()
            },
        )
        .unwrap();
        assert_eq!(comparison.functions.moved.len(), 1);
        assert_eq!(
            comparison.functions.moved[0].match_basis,
            "body_fingerprint"
        );
        assert_eq!(
            comparison.functions.moved[0].before_path.as_deref(),
            Some("src/old.rs")
        );
        assert_eq!(
            comparison.functions.moved[0].after_path.as_deref(),
            Some("src/new.rs")
        );
    }

    #[test]
    fn leaves_ambiguous_body_matches_unpaired() {
        let mut first = function("first", 10, 0);
        first.body_fingerprint = "shared-body".to_owned();
        let mut second = function("second", 10, 0);
        second.body_fingerprint = "shared-body".to_owned();
        let mut replacement = function("replacement", 10, 0);
        replacement.body_fingerprint = "shared-body".to_owned();
        let before = report(
            "/repo",
            20,
            vec![file("src/lib.rs", vec![first, second])],
            0,
            0,
        );
        let after = report(
            "/repo",
            10,
            vec![file("src/lib.rs", vec![replacement])],
            0,
            0,
        );
        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap();
        assert_eq!(comparison.functions.removed.len(), 2);
        assert_eq!(comparison.functions.added.len(), 1);
        assert!(
            comparison
                .warnings
                .iter()
                .any(|warning| warning.contains("ambiguous"))
        );
    }

    #[test]
    fn lambda_insertion_does_not_remap_siblings() {
        let make_lambda = |name: &str, body: &str| {
            let mut function = lambda(name, 10);
            function.snapshot_id = format!("snapshot-{name}");
            function.body_fingerprint = body.to_owned();
            function.declaration_fingerprint = format!("declaration-{name}");
            function
        };
        let before = report(
            "/repo",
            20,
            vec![file(
                "module.py",
                vec![
                    make_lambda("first", "body-first"),
                    make_lambda("second", "body-second"),
                ],
            )],
            0,
            0,
        );
        let after = report(
            "/repo",
            30,
            vec![file(
                "module.py",
                vec![
                    make_lambda("inserted", "body-inserted"),
                    make_lambda("first", "body-first"),
                    make_lambda("second", "body-second"),
                ],
            )],
            0,
            0,
        );
        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap();
        assert_eq!(comparison.functions.added.len(), 1);
        assert!(comparison.functions.changed.is_empty());
        assert!(comparison.functions.removed.is_empty());
    }

    #[test]
    fn language_deltas_partition_repository_components() {
        let rust_function = function("rust", 10, 2);
        let python_function = function("python", 20, 3);
        let before = report(
            "/repo",
            30,
            vec![
                file_with_language("src/lib.rs", "rust", vec![rust_function]),
                file_with_language("src/tool.py", "python", vec![python_function]),
            ],
            0,
            0,
        );
        let comparison = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before.clone(),
            before,
            CompareOptions::default(),
        )
        .unwrap();
        assert_eq!(comparison.language_deltas.len(), 2);
        assert_eq!(
            comparison
                .language_deltas
                .iter()
                .map(|delta| delta.burden.total.before)
                .sum::<usize>(),
            comparison.burden.total.before
        );
        assert_eq!(
            comparison
                .language_deltas
                .iter()
                .flat_map(|delta| delta.components.iter())
                .filter(|component| component.name == "boundary")
                .map(|component| component.before)
                .sum::<usize>(),
            comparison
                .components
                .iter()
                .find(|component| component.name == "boundary")
                .map_or(0, |component| component.before)
        );
    }

    #[test]
    fn rejects_reports_without_analysis_evidence() {
        let mut before = report(
            "/repo",
            10,
            vec![file("src/lib.rs", vec![function("run", 10, 0)])],
            0,
            0,
        );
        before.schema_version.clear();
        let after = before.clone();
        let error = compare_reports(
            Path::new("before.json"),
            Path::new("after.json"),
            before,
            after,
            CompareOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("compact or non-analysis"));
    }
}
