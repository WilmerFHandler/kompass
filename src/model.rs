use serde::Serialize;

/// The historical identifier for Kompass's first scoring model. The current
/// default is structural-v3; this constant remains stable for v1 consumers.
pub const SCORE_MODEL: &str = "structural-v1";
pub const STRUCTURAL_V1: &str = "structural-v1";
pub const STRUCTURAL_V2: &str = "structural-v2";
pub const STRUCTURAL_V3: &str = "structural-v3";

/// A selectable scoring model. The default is the experimental v3 model;
/// callers can select v1 or v2 explicitly when they need historical scores.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScoringModel {
    StructuralV1,
    StructuralV2,
    #[default]
    StructuralV3,
}

impl ScoringModel {
    pub const fn label(self) -> &'static str {
        match self {
            Self::StructuralV1 => STRUCTURAL_V1,
            Self::StructuralV2 => STRUCTURAL_V2,
            Self::StructuralV3 => STRUCTURAL_V3,
        }
    }
}

/// A machine-readable analysis report. JSON output serializes this structure
/// directly, so adding fields should remain backwards-compatible.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub tool: String,
    pub version: String,
    pub model: String,
    pub root: String,
    pub summary: Summary,
    pub coverage: Coverage,
    pub macro_opacity: MacroOpacity,
    pub files: Vec<FileReport>,
    pub errors: Vec<AnalysisError>,
}

impl Report {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub files: usize,
    pub code_lines: usize,
    pub tokens: usize,
    pub production: CategorySummary,
    pub test: CategorySummary,
    /// Sum of the exclusive callable units in every analyzed file.
    pub burden: Burden,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CategorySummary {
    pub functions: usize,
    pub total_score: usize,
    pub average_score: f64,
    pub p95_score: usize,
    pub highest_score: usize,
}

/// Aggregate complexity in exact score units. For structural-v1 one unit is
/// one score point; for structural-v2 and structural-v3 one unit is one tenth
/// of a point.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Burden {
    pub production: usize,
    pub test: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Coverage {
    pub discovered_files: usize,
    pub analyzed_files: usize,
    pub failed_files: usize,
    pub test_files: usize,
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileReport {
    pub path: String,
    pub lines: LineCounts,
    pub tokens: usize,
    pub functions: Vec<FunctionReport>,
    /// Sum of each callable's score, with nested callable bodies counted only
    /// in their own report.
    pub burden: Burden,
    /// Source macro invocations in this file. Expansion is intentionally
    /// excluded from the score, so this remains a visible opacity signal.
    pub macro_opacity: MacroOpacity,
}

/// Macro coverage that can be measured from the source currently on disk.
/// `source_tokens` counts lexical tokens in each invocation span, including
/// the macro path and delimiters, without expanding the invocation.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MacroOpacity {
    pub invocations: usize,
    pub source_tokens: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LineCounts {
    pub total: usize,
    pub code: usize,
    pub comments: usize,
    pub blank: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct FunctionReport {
    pub name: String,
    pub kind: FunctionKind,
    pub category: Category,
    pub location: Location,
    pub lines: usize,
    /// Rust lexical tokens in the complete function span, including public
    /// visibility when present. Comments and whitespace are excluded, outer
    /// attributes are excluded, and macro contents are counted as written.
    pub tokens: usize,
    pub metrics: Metrics,
    pub score: Score,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Production,
    Test,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionKind {
    Function,
    Method,
    TraitMethod,
    NestedFunction,
    Closure,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Location {
    pub start: Position,
    pub end: Position,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Position {
    /// One-based source line.
    pub line: usize,
    /// One-based UTF-8 byte column.
    pub column: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Metrics {
    pub code_lines: usize,
    pub statements: usize,
    /// Count of expression-level operations used by structural-v3. Bindings,
    /// paths, literals, fields, borrows, grouping, and parentheses are not
    /// operations by themselves.
    pub expression_operations: usize,
    pub decisions: usize,
    /// Control-flow decisions excluding short-circuit boolean operators.
    pub control_decisions: usize,
    pub nesting_penalty: usize,
    pub max_depth: usize,
    pub macro_calls: usize,
    pub branches: usize,
    pub boolean_operators: usize,
    pub match_arms: usize,
    pub loops: usize,
    pub returns: usize,
    pub mutations: usize,
    pub parameters: usize,
    /// Signature inputs excluding a method receiver.
    pub explicit_parameters: usize,
    pub call_sites: usize,
    pub closures: usize,
    pub unsafe_blocks: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Score {
    /// An unbounded integer score. Higher values indicate more structural
    /// signals in the function.
    pub value: usize,
    /// Exact score units. This is equal to `value`; the explicit name makes
    /// the tenths convention of structural-v2 and structural-v3 unambiguous in
    /// JSON.
    pub units: usize,
    /// Human-readable score, formatted according to the selected model.
    pub display: String,
    pub decisions: usize,
    /// The v2 and v3 control-decision count, excluding boolean operators.
    pub control_decisions: usize,
    pub nesting_penalty: usize,
    pub statement_penalty: usize,
    pub boundary: usize,
    pub boolean_operator_units: usize,
    pub call_site_units: usize,
    pub parameter_units: usize,
    pub match_arm_units: usize,
    /// Structural-v3's one-tenth-unit charge for each expression operation.
    pub expression_operation_units: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct AnalysisError {
    pub path: Option<String>,
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Read,
    Parse,
    Lex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    Text,
    Json,
}

/// The primary key used for the text hotspot ranking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortBy {
    Score,
    Depth,
    Size,
}

impl SortBy {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Score => "score",
            Self::Depth => "depth",
            Self::Size => "size",
        }
    }
}
