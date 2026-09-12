use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use kompass::{OutputFormat, SortBy, analyze, discover, output};

#[derive(Debug, Parser)]
#[command(
    name = "kompass",
    version,
    about = "Find the Rust code that takes the most thought to change",
    long_about = "Analyze Rust source and rank structural hotspots that may take more thought to understand and change.",
    after_help = "Use `kompass --help` for the agent workflow and JSON field meanings.",
    after_long_help = r#"
Agent workflow:
  1. Save a machine-readable baseline for one stable scope:
       kompass --format json PATH > before.json
  2. After a behavior-preserving refactor, rerun the same PATH:
       kompass --format json PATH > after.json
     Run the relevant behavior tests separately; a score comparison cannot
     prove that behavior is preserved.
  3. Compare integer fields in JSON. `summary.burden.production` is the
     production burden in integer tenths (143 means 14.3). Inspect
     `files[].burden`, `files[].functions[].score.value`, and
     `files[].functions[].metrics` for file and function detail.

JSON is the agent interface: it includes every analyzed function. `--top`,
`--sort`, `--tests`, and `--all` affect text output only. Production and test
categories use the same score but remain scored and summarized separately.
Only compare reports whose top-level `model` values match.
Check `coverage.complete` and `errors`; status 0 means the report completed
without analysis errors, while status 2 means the input, analysis, or output
failed. Status 2 can still emit a partial JSON report, so a lower burden is
inconclusive when coverage falls or macro opacity rises.

`macro_opacity.invocations` and `macro_opacity.source_tokens` count macro
source as written, without expansion, including built-in macros. Compare them
with burden because a lower score is a review signal, not proof that code is
cleaner or correct. Semantic module boundaries and coupling are not measured;
review those manually and do not blindly minimize the score.
"#
)]
struct Cli {
    /// Rust file, directory, or Cargo workspace to analyze.
    #[arg(value_name = "PATH", default_value = ".")]
    path: PathBuf,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Show the test ranking instead of the production ranking.
    #[arg(long, conflicts_with = "all")]
    tests: bool,

    /// Show both production and test rankings.
    #[arg(long)]
    all: bool,

    /// Number of functions to show in the text report.
    #[arg(long, default_value_t = 10, value_name = "N")]
    top: usize,

    /// Primary ordering for text hotspot rankings.
    #[arg(long, value_enum, default_value_t = Sort::Score)]
    sort: Sort,
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
    let discovered = match discover::discover(&cli.path) {
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
