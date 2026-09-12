use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use kompass::{OutputFormat, SortBy, analyze, diff, discover, output};

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
     burden, p95, highest-callable, per-file, and possible-redistribution deltas.

For custom automation, `summary.burden.production` is the production burden in
integer tenths (143 means 14.3). Inspect `files[].burden`,
`files[].functions[].score.value`, and `files[].functions[].metrics` for file
and callable detail. `summary.languages` counts Rust and Python files, while
`analysis_contract` records the frontend and discovery contracts used.

JSON is the agent interface: it includes every analyzed callable and initializer. `--top`,
`--sort`, `--tests`, and `--all` affect text output only. Production and test
categories use the same score but remain scored and summarized separately.
Only compare reports whose top-level `model` and `analysis_contract` values match.
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
scope, and complete coverage. Pass `--allow-file-changes` only when added or removed files are part
of the intended comparison; the output then highlights those files and warns
that aggregate deltas include their burden.
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

    /// Allow added or removed files, with an explicit scope warning.
    #[arg(long)]
    allow_file_changes: bool,
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

    if let Some(Command::Diff(args)) = cli.command {
        let comparison = match diff::compare_paths_with_options(
            &args.before,
            &args.after,
            diff::CompareOptions {
                allow_file_changes: args.allow_file_changes,
            },
        ) {
            Ok(comparison) => comparison,
            Err(error) => {
                eprintln!("kompass: {error}");
                std::process::exit(2);
            }
        };
        let rendered = match diff::render_comparison(&comparison, args.format.into()) {
            Ok(rendered) => rendered,
            Err(error) => {
                eprintln!("kompass: could not render comparison: {error}");
                std::process::exit(2);
            }
        };
        let mut stdout = io::BufWriter::new(io::stdout().lock());
        match stdout
            .write_all(rendered.as_bytes())
            .and_then(|_| stdout.flush())
        {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return,
            Err(error) => {
                eprintln!("kompass: {error}");
                std::process::exit(2);
            }
        }
    }

    let discovered = match discover::discover_with_language(&cli.path, cli.language.into()) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("kompass: {error}");
            std::process::exit(2);
        }
    };

    let report = analyze::analyze(&cli.path, discovered);
    match output::write_report(
        &report,
        cli.format.into(),
        cli.top,
        cli.tests,
        cli.all,
        cli.sort.into(),
    ) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return,
        Err(error) => {
            eprintln!("kompass: {error}");
            std::process::exit(2);
        }
    }

    if report.has_errors() {
        std::process::exit(2);
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
