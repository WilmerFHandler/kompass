use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use kompass::{OutputFormat, SortBy, analyze, diff, discover, explain, output};

#[derive(Debug, Parser)]
#[command(
    name = "kompass",
    version,
    about = "Find the code that takes the most thought to change",
    long_about = "Analyze Rust and Python source and rank structural hotspots that may take more thought to understand and change.",
    after_help = "Use `kompass --help` for the agent workflow and JSON field meanings.",
    after_long_help = r#"
Agent workflow:
  1. Save a machine-readable baseline for one stable scope:
       kompass --format json PATH > before.json
  2. After a behavior-preserving refactor, rerun the same PATH:
       kompass --format json PATH > after.json
     Run the relevant behavior tests separately; a score comparison cannot
     prove that behavior is preserved.
  3. Compare the reports safely:
       kompass diff before.json after.json
     The command checks model, analysis contract, root, file scope, and coverage before showing
     burden, p95, highest-callable, per-file, per-language, and callable identity deltas.

For custom automation, `summary.burden.production` is the production burden in
integer tenths (143 means 14.3). Inspect `files[].burden`,
`files[].functions[].score.value`, and `files[].functions[].metrics` for file
and callable detail. `summary.languages` counts Rust and Python files, and
`summary.by_language` contains their independent aggregates. `scope` records
the selection, requested language filter, and category policy, while
`analysis_contract` records the frontend and discovery contracts used.

JSON is the agent interface: it includes every analyzed callable and initializer. `--top`,
`--sort`, `--tests`, and `--all` affect text output only. Production and test
categories use the same score but remain scored and summarized separately.
Only compare reports whose top-level `model` and `analysis_contract` values match; schema and evidence values must also be current.
Check `coverage.complete` and `errors`; status 0 means the report completed
without analysis errors, while status 2 means the input, analysis, or output
failed. Status 2 can still emit a partial JSON report, so a lower burden is
inconclusive when coverage falls or macro opacity rises.

`macro_opacity.invocations` and `macro_opacity.source_tokens` count macro
invocation source as written without expansion, including built-in macros.
`macro_opacity.definitions` and `macro_opacity.definition_tokens` expose macro
rule bodies separately. Compare them with burden; lower score is a review signal,
not proof that code is cleaner or correct. Semantic module boundaries and coupling
are not measured; review those manually and do not blindly minimize the score.

Use `--format json --compact --top N` for a bounded machine-readable analysis
view. It is marked `report_kind: "analysis_compact"` and retains full scope,
coverage, and aggregate values alongside explicit `returned` and `total` counts.
Use `kompass explain PATH --line N` to inspect every callable containing a line;
the output marks the innermost candidate, shows all eight score components and
their source locations, and includes relevant source-only call, reachability,
and duplicate evidence from the freshly analyzed report.

`kompass diff BEFORE.json AFTER.json --format json --compact --top N` uses the
same explicit truncation contract with `report_kind: "diff_compact"` and retains
the evidence contract version in its before and after metadata.

Const and static initializers appear in `files[].functions[]` as callable-like
`const_initializer` or `static_initializer` units. Their initializer
expressions are scored exclusively, so moving control flow out of a function
into an initializer remains visible without counting the same body twice.
Closures retain the surrounding lexical control-flow depth in their own score,
so extracting a nested branch into an immediately-created closure does not
erase its nesting context.

Python functions, async functions, methods, nested functions, and lambdas are
separate units. Executable module and class bodies are initializer units, but
empty files, docstrings, pass statements, and declarations alone add no boundary.
Nested Python functions and lambdas retain their lexical control-flow depth.
Python input must be UTF-8 without a byte-order mark; unsupported encodings are
reported as file errors so coverage cannot appear complete.

Compare complete reports with:
     kompass diff BEFORE.json AFTER.json
The diff command requires the same root, score model, analysis contract, file
scope, and complete coverage. Pass --allow-root-change for equivalent checkouts
or worktrees with different absolute roots; relative file paths, language,
schema, evidence, and coverage checks remain enforced. Pass --allow-file-changes
only when added or removed files are part of the intended comparison; the output
then highlights those files and warns that aggregate deltas include their burden.
Every callable has a snapshot id and declaration/body lexical fingerprints.
Diff matching proceeds through unique snapshot, declaration/body, body,
declaration, and qualified-name evidence. Ambiguous evidence remains unmatched
and is reported as a warning; ordinal or score-based matching is never used.
Moved callables appear in the functions.moved collection, while language_deltas
partitions burden and score components by language.
The comparison also shows production and test callable-count, p95, and highest
score changes, per-file burden deltas, and conservative possible-redistribution
signals. A redistribution signal proves no call or extraction relationship.
"#
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Rust or Python file, directory, or Cargo workspace to analyze.
    #[arg(value_name = "PATH", default_value = ".")]
    path: PathBuf,

    /// Source language to include when analyzing a directory.
    #[arg(long, value_enum, default_value_t = Language::All)]
    language: Language,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Return only the top N callables in an explicitly marked compact JSON view.
    #[arg(long)]
    compact: bool,

    /// Show the test ranking instead of the production ranking.
    #[arg(long, conflicts_with = "all")]
    tests: bool,

    /// Show both production and test rankings.
    #[arg(long)]
    all: bool,

    /// Number of callables and initializer units to show in the text report.
    #[arg(long, default_value_t = 10, value_name = "N")]
    top: usize,

    /// Primary ordering for text hotspot rankings.
    #[arg(long, value_enum, default_value_t = Sort::Score)]
    sort: Sort,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compare two complete JSON reports from the same analyzed scope.
    Diff(DiffArgs),

    /// Explain every callable containing a source line.
    Explain(ExplainArgs),
}

#[derive(Debug, Args)]
struct DiffArgs {
    /// JSON report captured before a refactor.
    #[arg(value_name = "BEFORE")]
    before: PathBuf,

    /// JSON report captured after a refactor.
    #[arg(value_name = "AFTER")]
    after: PathBuf,

    /// Output format for the comparison.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Return only the top N changes in an explicitly marked compact JSON view.
    #[arg(long)]
    compact: bool,

    /// Number of callable changes to return in compact output.
    #[arg(long, default_value_t = 10, value_name = "N")]
    top: usize,

    /// Allow added or removed files, with an explicit scope warning.
    #[arg(long)]
    allow_file_changes: bool,

    /// Allow different absolute report roots while keeping relative scope checks.
    #[arg(long)]
    allow_root_change: bool,
}

#[derive(Debug, Args)]
struct ExplainArgs {
    /// Source file whose containing callables should be reported.
    #[arg(value_name = "PATH")]
    path: PathBuf,

    /// One-based source line to explain.
    #[arg(long, value_name = "N")]
    line: usize,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Format {
    Text,
    Json,
}

impl From<Format> for OutputFormat {
    fn from(format: Format) -> Self {
        match format {
            Format::Text => Self::Text,
            Format::Json => Self::Json,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Sort {
    Score,
    Depth,
    Size,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Language {
    All,
    Rust,
    Python,
}

impl From<Language> for discover::LanguageFilter {
    fn from(language: Language) -> Self {
        match language {
            Language::All => Self::All,
            Language::Rust => Self::Rust,
            Language::Python => Self::Python,
        }
    }
}

impl From<Sort> for SortBy {
    fn from(sort: Sort) -> Self {
        match sort {
            Sort::Score => Self::Score,
            Sort::Depth => Self::Depth,
            Sort::Size => Self::Size,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(false) => {}
        Ok(true) => std::process::exit(2),
        Err(error) => {
            eprintln!("kompass: {error}");
            std::process::exit(2);
        }
    }
}

fn run(cli: &Cli) -> Result<bool, String> {
    match cli.command.as_ref() {
        Some(Command::Diff(args)) => run_diff(args),
        Some(Command::Explain(args)) => run_explain(args),
        None => run_analysis(cli),
    }
}

fn run_diff(args: &DiffArgs) -> Result<bool, String> {
    let comparison = diff::compare_paths_with_options(
        &args.before,
        &args.after,
        diff::CompareOptions {
            allow_file_changes: args.allow_file_changes,
            allow_root_change: args.allow_root_change,
        },
    )
    .map_err(|error| error.to_string())?;
    let rendered = if args.compact {
        diff::render_compact_comparison(&comparison, args.format.into(), args.top)
    } else {
        diff::render_comparison(&comparison, args.format.into())
    }
    .map_err(|error| format!("could not render comparison: {error}"))?;
    write_stdout(&rendered)?;
    Ok(false)
}

fn run_explain(args: &ExplainArgs) -> Result<bool, String> {
    let discovered = discover::discover_with_language(&args.path, discover::LanguageFilter::All)
        .map_err(|error| error.to_string())?;
    let report = analyze::analyze(&args.path, discovered);
    let explanation = explain::explain_report(&report, &args.path, args.line)
        .map_err(|error| error.to_string())?;
    let rendered = output::render_explain(&explanation, args.format.into())
        .map_err(|error| format!("could not render explanation: {error}"))?;
    write_stdout(&rendered)?;
    Ok(report.has_errors())
}

fn run_analysis(cli: &Cli) -> Result<bool, String> {
    if cli.compact && cli.format != Format::Json {
        return Err("--compact requires --format json".to_owned());
    }

    let discovered = discover::discover_with_language(&cli.path, cli.language.into())
        .map_err(|error| error.to_string())?;

    let report = analyze::analyze(&cli.path, discovered);
    let rendered = if cli.compact {
        output::render_compact_analysis(
            &explain::compact_analysis(&report, cli.top, cli.tests, cli.all, cli.sort.into()),
            OutputFormat::Json,
        )
        .map_err(|error| io::Error::other(format!("could not render report: {error}")))
    } else {
        output::render_report(
            &report,
            cli.format.into(),
            cli.top,
            cli.tests,
            cli.all,
            cli.sort.into(),
        )
        .map_err(|error| io::Error::other(format!("could not render report: {error}")))
    };
    let rendered = rendered.map_err(|error| error.to_string())?;
    write_stdout(&rendered)?;
    Ok(report.has_errors())
}

fn write_stdout(rendered: &str) -> Result<(), String> {
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    match stdout
        .write_all(rendered.as_bytes())
        .and_then(|_| stdout.flush())
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_and_all_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["kompass", "--tests", "--all"]).is_err());
    }
}
