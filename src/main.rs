use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use kompass::{AnalysisOptions, OutputFormat, ScoringModel, SortBy, analyze, discover, output};

#[derive(Debug, Parser)]
#[command(
    name = "kompass",
    version,
    about = "Find the Rust code that takes the most thought to change"
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

    /// Scoring model. structural-v3 is the default; v1 and v2 remain selectable.
    #[arg(long, value_enum, default_value_t = Model::StructuralV3)]
    model: Model,
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
enum Model {
    #[value(name = "structural-v1")]
    StructuralV1,
    #[value(name = "structural-v2")]
    StructuralV2,
    #[value(name = "structural-v3")]
    StructuralV3,
}

impl From<Model> for ScoringModel {
    fn from(model: Model) -> Self {
        match model {
            Model::StructuralV1 => Self::StructuralV1,
            Model::StructuralV2 => Self::StructuralV2,
            Model::StructuralV3 => Self::StructuralV3,
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
    let options = AnalysisOptions {
        model: cli.model.into(),
    };

    let discovered = match discover::discover(&cli.path) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("kompass: {error}");
            std::process::exit(2);
        }
    };

    let report = analyze::analyze(&cli.path, discovered, options);
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
