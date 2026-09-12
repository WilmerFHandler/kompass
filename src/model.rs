use serde::{Deserialize, Serialize};

/// Stable identifier for the scoring formula embedded in each report.
pub const SCORE_MODEL: &str = "structural-v4";

/// Stable identity of the source discovery contract used by a report.
pub const DISCOVERY_CONTRACT: &str = "multi-language-v1";

/// Stable identity of the language frontend contract used by a report.
///
/// The implementation behind a frontend may evolve while this contract stays
/// fixed only when its serialized `FileAnalysis` semantics remain compatible.
pub const FRONTEND_CONTRACT: &str = "rust-syn-v1;python-ruff-0.0.10-py314";

/// Stable identity of the complete analysis contract. It intentionally keeps
/// the score model separate so consumers can tell formula changes from parser
/// or discovery changes.
pub const ANALYSIS_CONTRACT: &str = "analysis-v1";

/// A machine-readable analysis report. JSON output serializes this structure
/// directly, so adding fields should remain backwards-compatible.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub tool: String,
    pub version: String,
    pub model: String,
    pub analysis_contract: AnalysisContract,
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
    pub languages: LanguageCounts,
    pub production: CategorySummary,
    pub test: CategorySummary,
    /// Sum of the exclusive callable units in every analyzed file.
    pub burden: Burden,
}

/// Number of analyzed files per source language.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LanguageCounts {
    pub rust: usize,
    pub python: usize,
}

/// Versioned contract for discovery and frontend semantics.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnalysisContract {
    pub version: String,
    pub frontend: String,
    pub discovery: String,
}

impl AnalysisContract {
    pub fn current() -> Self {
        Self {
            version: ANALYSIS_CONTRACT.to_owned(),
            frontend: FRONTEND_CONTRACT.to_owned(),
            discovery: DISCOVERY_CONTRACT.to_owned(),
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.version.trim().is_empty()
            && !self.frontend.trim().is_empty()
            && !self.discovery.trim().is_empty()
    }

    pub fn display(&self) -> String {
        format!(
            "version={} frontend={} discovery={}",
            self.version, self.frontend, self.discovery
        )
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CategorySummary {
    pub functions: usize,
    pub total_score: usize,
    pub average_score: f64,
    pub p95_score: usize,
    pub highest_score: usize,
}

/// Aggregate complexity in exact integer-tenth score units.
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
    pub language: Language,
    pub lines: LineCounts,
    pub tokens: usize,
    pub functions: Vec<FunctionReport>,
    /// Sum of each callable's score, with nested callable bodies counted only
    /// in their own report.
    pub burden: Burden,
    /// Source macro invocations and definitions in this file. Expansion is
    /// intentionally excluded from the score, so both remain visible opacity
    /// signals.
    pub macro_opacity: MacroOpacity,
}

/// Language-neutral result returned by a source frontend before the analyzer
/// adds path, language, and aggregate metadata.
#[derive(Clone, Debug)]
pub struct FileAnalysis {
    pub lines: LineCounts,
    pub tokens: usize,
    pub functions: Vec<FunctionReport>,
    pub macro_opacity: MacroOpacity,
}

/// Macro coverage that can be measured from the source currently on disk.
/// `source_tokens` counts lexical tokens in each invocation span, including
/// the macro path and delimiters, without expanding the invocation.
/// `definition_tokens` counts lexical tokens in macro definitions, including
/// their rule bodies, without expanding or parsing those rules as Rust code.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MacroOpacity {
    pub invocations: usize,
    pub source_tokens: usize,
    /// Number of macro definitions found in source.
    pub definitions: usize,
    /// Lexical tokens in macro definitions, kept separate from invocation
    /// source tokens so changing a rule body remains visible.
    pub definition_tokens: usize,
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
    /// Source tokens in the complete callable span. Each language frontend
    /// defines its lexical token contract; comments and whitespace are excluded.
    pub tokens: usize,
    pub metrics: Metrics,
    pub score: Score,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Production,
    Test,
}

/// Source language represented in a report or discovered file.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    Python,
}

impl Language {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
        }
    }

    pub const fn serialized(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionKind {
    Function,
    Method,
    TraitMethod,
    NestedFunction,
    Closure,
    Lambda,
    ModuleInitializer,
    ClassInitializer,
    ConstInitializer,
    StaticInitializer,
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
    /// Count of expression-level operations used by the structural score. Bindings,
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
    /// Exact integer-tenth score units. This is equal to `value` and is the
    /// preferred field for machine-readable comparisons.
    pub units: usize,
    /// Human-readable score, formatted in score points.
    pub display: String,
    pub decisions: usize,
    /// Control-decision count, excluding boolean operators.
    pub control_decisions: usize,
    pub nesting_penalty: usize,
    pub boundary: usize,
    pub boolean_operator_units: usize,
    pub call_site_units: usize,
    pub parameter_units: usize,
    pub match_arm_units: usize,
    /// One-tenth-unit charge for each expression operation.
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
