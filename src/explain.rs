//! Explain and compact machine-readable views over a freshly analyzed report.
//!
//! The analyzer remains the source of truth for scores. This module only
//! selects containing units, groups the score's fixed components, and projects
//! a bounded view while retaining the complete scope and aggregate context.

use std::cmp::Ordering;
use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::diff::{
    BurdenDelta, CallableStatsDelta, Comparison, ComponentDelta, FileBurdenDelta, FileChanges,
    FunctionChange, MacroOpacityDelta, RedistributionHint, ScopeSummary,
};
use crate::model::{
    AnalysisContract, AnalysisError, Category, Coverage, FileReport, FunctionKind, FunctionReport,
    Language, LanguageCounts, Location, MacroOpacity, Report, SCHEMA_VERSION, Score,
    ScoreComponent, Summary,
};

/// Evidence is intentionally an explicit placeholder until a semantic
/// evidence module can provide call-graph, duplication, or behavior links.
#[derive(Clone, Debug, Serialize)]
pub struct EvidencePlaceholder {
    pub kind: String,
    pub status: String,
    pub detail: String,
}

fn unavailable_evidence() -> Vec<EvidencePlaceholder> {
    vec![EvidencePlaceholder {
        kind: "semantic-evidence".to_owned(),
        status: "unavailable".to_owned(),
        detail: "No evidence module is configured; this explanation is syntax-only.".to_owned(),
    }]
}

/// The complete scope context shared by compact analysis output.
#[derive(Clone, Debug, Serialize)]
pub struct AnalysisScope {
    pub root: String,
    pub files: usize,
    pub discovered_files: usize,
    pub analyzed_files: usize,
    pub languages: LanguageCounts,
}

#[derive(Clone, Debug, Serialize)]
pub struct AggregateSnapshot {
    pub summary: Summary,
    pub macro_opacity: MacroOpacity,
}

/// A bounded analysis view. `returned` and `total` make truncation explicit;
/// consumers must never mistake this for a full report.
#[derive(Clone, Debug, Serialize)]
pub struct CompactAnalysis {
    pub report_kind: String,
    pub schema_version: u32,
    pub tool: String,
    pub version: String,
    pub model: String,
    pub analysis_contract: AnalysisContract,
    pub scope: AnalysisScope,
    pub coverage: Coverage,
    pub aggregates: AggregateSnapshot,
    pub selected_categories: Vec<Category>,
    pub returned: usize,
    pub total: usize,
    pub functions: Vec<CompactFunction>,
    pub errors: Vec<AnalysisError>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompactFunction {
    pub path: String,
    pub language: Language,
    pub name: String,
    pub kind: FunctionKind,
    pub category: Category,
    pub location: Location,
    pub lines: usize,
    pub tokens: usize,
    pub score: CompactScore,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompactScore {
    pub units: usize,
    pub display: String,
}

/// Build a bounded analysis projection while preserving all aggregate values.
pub fn compact_analysis(
    report: &Report,
    top: usize,
    tests: bool,
    all: bool,
    sort: crate::model::SortBy,
) -> CompactAnalysis {
    let selected_categories = selected_categories(tests, all);
    let mut functions = report
        .files
        .iter()
        .flat_map(|file| {
            file.functions
                .iter()
                .filter(|function| selected_categories.contains(&function.category))
                .map(move |function| (file, function))
        })
        .collect::<Vec<_>>();
    functions.sort_by(|(left_file, left), (right_file, right)| {
        compare_functions(
            sort,
            left_file.path.as_str(),
            left,
            right_file.path.as_str(),
            right,
        )
    });
    let total = functions.len();
    let functions = functions
        .into_iter()
        .take(top)
        .map(|(file, function)| compact_function(file, function))
        .collect::<Vec<_>>();

    CompactAnalysis {
        report_kind: "analysis_compact".to_owned(),
        schema_version: SCHEMA_VERSION,
        tool: report.tool.clone(),
        version: report.version.clone(),
        model: report.model.clone(),
        analysis_contract: report.analysis_contract.clone(),
        scope: analysis_scope(report),
        coverage: report.coverage.clone(),
        aggregates: AggregateSnapshot {
            summary: report.summary.clone(),
            macro_opacity: report.macro_opacity.clone(),
        },
        selected_categories,
        returned: functions.len(),
        total,
        functions,
        errors: report.errors.clone(),
    }
}

fn analysis_scope(report: &Report) -> AnalysisScope {
    AnalysisScope {
        root: report.root.clone(),
        files: report.summary.files,
        discovered_files: report.coverage.discovered_files,
        analyzed_files: report.coverage.analyzed_files,
        languages: report.summary.languages.clone(),
    }
}

fn compact_function(file: &FileReport, function: &FunctionReport) -> CompactFunction {
    CompactFunction {
        path: file.path.clone(),
        language: file.language,
        name: function.name.clone(),
        kind: function.kind.clone(),
        category: function.category,
        location: function.location.clone(),
        lines: function.lines,
        tokens: function.tokens,
        score: CompactScore {
            units: function.score.units,
            display: function.score.display.clone(),
        },
    }
}

fn selected_categories(tests: bool, all: bool) -> Vec<Category> {
    if all {
        vec![Category::Production, Category::Test]
    } else if tests {
        vec![Category::Test]
    } else {
        vec![Category::Production]
    }
}

fn compare_functions(
    sort: crate::model::SortBy,
    left_path: &str,
    left: &FunctionReport,
    right_path: &str,
    right: &FunctionReport,
) -> Ordering {
    let primary = match sort {
        crate::model::SortBy::Score => right.score.value.cmp(&left.score.value),
        crate::model::SortBy::Depth => right.metrics.max_depth.cmp(&left.metrics.max_depth),
        crate::model::SortBy::Size => right.tokens.cmp(&left.tokens),
    };
    primary
        .then_with(|| left_path.cmp(right_path))
        .then_with(|| left.location.start.line.cmp(&right.location.start.line))
        .then_with(|| left.location.start.column.cmp(&right.location.start.column))
        .then_with(|| left.name.cmp(&right.name))
}

/// A bounded diff projection. Aggregate deltas and scope metadata are kept in
/// full, while the changed callable list is explicitly truncated.
#[derive(Clone, Debug, Serialize)]
pub struct CompactDiff {
    pub report_kind: String,
    pub schema_version: u32,
    pub comparable: bool,
    pub tool: String,
    pub version: String,
    pub model: String,
    pub analysis_contract: AnalysisContract,
    pub scope: ScopeSummary,
    pub coverage: DiffCoverage,
    pub aggregates: DiffAggregates,
    pub before: crate::diff::ReportMetadata,
    pub after: crate::diff::ReportMetadata,
    pub file_changes: FileChanges,
    pub returned: usize,
    pub total: usize,
    pub functions: Vec<FunctionChange>,
    pub possible_redistributions: Vec<RedistributionHint>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiffCoverage {
    pub complete: bool,
    pub before_complete: bool,
    pub after_complete: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiffAggregates {
    pub burden: BurdenDelta,
    pub callables: CallableStatsDelta,
    pub components: Vec<ComponentDelta>,
    pub macro_opacity: MacroOpacityDelta,
    pub file_burdens: Vec<FileBurdenDelta>,
}

pub fn compact_diff(comparison: &Comparison, top: usize) -> CompactDiff {
    let mut functions = comparison
        .functions
        .changed
        .iter()
        .chain(&comparison.functions.added)
        .chain(&comparison.functions.removed)
        .cloned()
        .collect::<Vec<_>>();
    functions.sort_by(|left, right| {
        right
            .delta
            .unsigned_abs()
            .cmp(&left.delta.unsigned_abs())
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.name.cmp(&right.name))
    });
    let total = functions.len();
    functions.truncate(top);
    let complete = comparison.scope.coverage_complete;

    CompactDiff {
        report_kind: "diff_compact".to_owned(),
        schema_version: SCHEMA_VERSION,
        comparable: comparison.comparable,
        tool: "kompass".to_owned(),
        version: comparison.after.version.clone(),
        model: comparison.before.model.clone(),
        analysis_contract: comparison.before.analysis_contract.clone(),
        scope: comparison.scope.clone(),
        coverage: DiffCoverage {
            complete,
            before_complete: complete,
            after_complete: complete,
        },
        aggregates: DiffAggregates {
            burden: comparison.burden.clone(),
            callables: comparison.callables.clone(),
            components: comparison.components.clone(),
            macro_opacity: comparison.macro_opacity.clone(),
            file_burdens: comparison.file_burdens.clone(),
        },
        before: comparison.before.clone(),
        after: comparison.after.clone(),
        file_changes: comparison.file_changes.clone(),
        returned: functions.len(),
        total,
        functions,
        possible_redistributions: comparison.possible_redistributions.clone(),
        warnings: comparison.warnings.clone(),
    }
}

/// Explain every analyzed unit whose source span contains `line` in `path`.
/// The caller performs analysis immediately before invoking this function.
pub fn explain_report(
    report: &Report,
    input_path: &Path,
    line: usize,
) -> Result<ExplainReport, String> {
    if line == 0 {
        return Err("--line must be at least 1".to_owned());
    }
    let canonical = fs::canonicalize(input_path).unwrap_or_else(|_| input_path.to_path_buf());
    if !canonical.is_file() {
        return Err(format!(
            "explain expects a source file, but {} is not a file",
            input_path.display()
        ));
    }
    let target = display_target(&canonical, Path::new(&report.root));
    let file = report
        .files
        .iter()
        .find(|file| file.path == target)
        .or_else(|| {
            report.files.iter().find(|file| {
                Path::new(&file.path)
                    .file_name()
                    .is_some_and(|name| Some(name) == canonical.file_name())
            })
        })
        .ok_or_else(|| format!("no analyzed file record matches {}", input_path.display()))?;

    let mut containing = file
        .functions
        .iter()
        .filter(|function| {
            function.location.start.line <= line && line <= function.location.end.line
        })
        .collect::<Vec<_>>();
    containing.sort_by(|left, right| {
        left.location
            .start
            .line
            .cmp(&right.location.start.line)
            .then_with(|| left.location.start.column.cmp(&right.location.start.column))
            .then_with(|| right.location.end.line.cmp(&left.location.end.line))
            .then_with(|| right.location.end.column.cmp(&left.location.end.column))
            .then_with(|| left.name.cmp(&right.name))
    });
    let innermost_index = containing
        .iter()
        .enumerate()
        .min_by_key(|(_, function)| span_size(&function.location))
        .map(|(index, _)| index);
    let candidates = containing
        .into_iter()
        .enumerate()
        .map(|(index, function)| ExplainCandidate::new(function, Some(index) == innermost_index))
        .collect::<Vec<_>>();

    Ok(ExplainReport {
        report_kind: "explain".to_owned(),
        schema_version: SCHEMA_VERSION,
        tool: report.tool.clone(),
        version: report.version.clone(),
        model: report.model.clone(),
        analysis_contract: report.analysis_contract.clone(),
        scope: ExplainScope {
            root: report.root.clone(),
            path: target,
            line,
            files: report.summary.files,
        },
        coverage: report.coverage.clone(),
        aggregates: AggregateSnapshot {
            summary: report.summary.clone(),
            macro_opacity: report.macro_opacity.clone(),
        },
        returned: candidates.len(),
        total: candidates.len(),
        candidates,
        evidence: unavailable_evidence(),
        errors: report.errors.clone(),
    })
}

fn display_target(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn span_size(location: &Location) -> (usize, usize, usize, usize) {
    (
        location.end.line.saturating_sub(location.start.line),
        location.end.column.saturating_sub(location.start.column),
        location.start.line,
        location.start.column,
    )
}

#[derive(Clone, Debug, Serialize)]
pub struct ExplainScope {
    pub root: String,
    pub path: String,
    pub line: usize,
    pub files: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExplainReport {
    pub report_kind: String,
    pub schema_version: u32,
    pub tool: String,
    pub version: String,
    pub model: String,
    pub analysis_contract: AnalysisContract,
    pub scope: ExplainScope,
    pub coverage: Coverage,
    pub aggregates: AggregateSnapshot,
    pub returned: usize,
    pub total: usize,
    pub candidates: Vec<ExplainCandidate>,
    pub evidence: Vec<EvidencePlaceholder>,
    pub errors: Vec<AnalysisError>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExplainCandidate {
    pub name: String,
    pub kind: FunctionKind,
    pub category: Category,
    pub location: Location,
    pub lines: usize,
    pub tokens: usize,
    pub score: Score,
    pub innermost: bool,
    pub components: Vec<ExplainComponent>,
    pub evidence: Vec<EvidencePlaceholder>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExplainComponent {
    pub component: ScoreComponent,
    pub units: usize,
    pub locations: Vec<Location>,
}

impl ExplainCandidate {
    fn new(function: &FunctionReport, innermost: bool) -> Self {
        let components = ScoreComponent::ALL
            .into_iter()
            .map(|component| {
                let units = component_units(&function.score, component);
                let locations = function
                    .score
                    .contributions
                    .iter()
                    .filter(|contribution| {
                        contribution.component == component && contribution.units > 0
                    })
                    .map(|contribution| contribution.location.clone())
                    .collect::<Vec<_>>();
                ExplainComponent {
                    component,
                    units,
                    locations: if locations.is_empty() && units > 0 {
                        vec![function.location.clone()]
                    } else {
                        locations
                    },
                }
            })
            .collect::<Vec<_>>();
        Self {
            name: function.name.clone(),
            kind: function.kind.clone(),
            category: function.category,
            location: function.location.clone(),
            lines: function.lines,
            tokens: function.tokens,
            score: function.score.clone(),
            innermost,
            components,
            evidence: unavailable_evidence(),
        }
    }
}

fn component_units(score: &Score, component: ScoreComponent) -> usize {
    match component {
        ScoreComponent::Boundary => score.boundary,
        ScoreComponent::ControlDecisions => score.control_decisions.saturating_mul(10),
        ScoreComponent::NestingPenalty => score.nesting_penalty.saturating_mul(10),
        ScoreComponent::BooleanOperators => score.boolean_operator_units,
        ScoreComponent::ExpressionOperations => score.expression_operation_units,
        ScoreComponent::CallSites => score.call_site_units,
        ScoreComponent::ExplicitParameters => score.parameter_units,
        ScoreComponent::MatchArms => score.match_arm_units,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Position, ScoreContribution};

    fn sample_score() -> Score {
        let location = Location {
            start: Position { line: 1, column: 1 },
            end: Position { line: 4, column: 2 },
        };
        Score {
            value: 45,
            units: 45,
            boundary: 10,
            control_decisions: 1,
            nesting_penalty: 1,
            boolean_operator_units: 5,
            expression_operation_units: 3,
            call_site_units: 2,
            parameter_units: 2,
            match_arm_units: 1,
            contributions: vec![ScoreContribution {
                component: ScoreComponent::Boundary,
                units: 10,
                location,
            }],
            ..Score::default()
        }
    }

    #[test]
    fn component_projection_always_has_eight_components() {
        let score = sample_score();
        let names = ScoreComponent::ALL;
        assert_eq!(names.len(), 8);
        assert_eq!(component_units(&score, ScoreComponent::Boundary), 10);
    }

    #[test]
    fn span_order_prefers_the_smallest_containing_unit() {
        let outer = Location {
            start: Position { line: 1, column: 1 },
            end: Position {
                line: 10,
                column: 1,
            },
        };
        let inner = Location {
            start: Position { line: 3, column: 1 },
            end: Position { line: 4, column: 1 },
        };
        assert!(span_size(&inner) < span_size(&outer));
    }
}
