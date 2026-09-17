use std::cmp::Ordering;
use std::fmt::Write as FmtWrite;
use std::io::{self, Write};

use crate::model::{
    Category, CategorySummary, ErrorKind, FunctionReport, Language, LanguageCounts, OutputFormat,
    Report, SortBy,
};

pub fn write_report(
    report: &Report,
    format: OutputFormat,
    top: usize,
    tests: bool,
    all: bool,
    sort: SortBy,
) -> io::Result<()> {
    let rendered = render_report(report, format, top, tests, all, sort)
        .map_err(|error| io::Error::other(format!("could not render report: {error}")))?;
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    stdout.write_all(rendered.as_bytes())?;
    stdout.flush()
}

pub fn render_report(
    report: &Report,
    format: OutputFormat,
    top: usize,
    tests: bool,
    all: bool,
    sort: SortBy,
) -> Result<String, serde_json::Error> {
    match format {
        OutputFormat::Json => {
            let mut output = serde_json::to_string_pretty(report)?;
            output.push('\n');
            Ok(output)
        }
        OutputFormat::Text => Ok(render_text(report, top, tests, all, sort)),
    }
}

fn render_text(report: &Report, top: usize, tests: bool, all: bool, sort: SortBy) -> String {
    let mut output = String::new();
    writeln!(
        output,
        "Kompass {} · {} structural complexity · {}",
        report.version,
        language_header(report),
        report.model
    )
    .unwrap();
    writeln!(output, "{}", report.root).unwrap();
    writeln!(
        output,
        "{} files · {} code lines · {} tokens",
        report.summary.files, report.summary.code_lines, report.summary.tokens
    )
    .unwrap();
    let languages = effective_language_counts(report);
    writeln!(
        output,
        "Languages · {} Rust files · {} Python files",
        languages.rust, languages.python
    )
    .unwrap();
    render_category_summary(&mut output, "Production", &report.summary.production);
    render_category_summary(&mut output, "Tests", &report.summary.test);
    writeln!(
        output,
        "Repository burden · {} total · {} production · {} tests",
        format_score(report.summary.burden.total),
        format_score(report.summary.burden.production),
        format_score(report.summary.burden.test)
    )
    .unwrap();
    writeln!(
        output,
        "Macro opacity · {} source invocations · {} invocation source tokens · {} definitions · {} definition tokens · unexpanded and excluded from score",
        report.macro_opacity.invocations,
        report.macro_opacity.source_tokens,
        report.macro_opacity.definitions,
        report.macro_opacity.definition_tokens
    )
    .unwrap();
    render_file_burdens(&mut output, report, top);

    if report.coverage.complete {
        writeln!(
            output,
            "Coverage complete · {} of {} files analyzed",
            report.coverage.analyzed_files, report.coverage.discovered_files
        )
        .unwrap();
    } else {
        writeln!(
            output,
            "Coverage partial · {} of {} files analyzed",
            report.coverage.analyzed_files, report.coverage.discovered_files
        )
        .unwrap();
    }
    if report.coverage.test_files > 0 {
        writeln!(
            output,
            "Test files · {} source files",
            report.coverage.test_files
        )
        .unwrap();
    }

    let categories = if all {
        vec![Category::Production, Category::Test]
    } else if tests {
        vec![Category::Test]
    } else {
        vec![Category::Production]
    };
    let has_selected_functions = categories
        .iter()
        .any(|category| !functions_for_category(report, *category).is_empty());
    for category in categories {
        let mut functions = functions_for_category(report, category);
        functions.sort_by(|(left_path, left), (right_path, right)| {
            compare_functions(sort, left_path, left, right_path, right)
        });
        if functions.is_empty() || top == 0 {
            continue;
        }
        output.push('\n');
        writeln!(output, "{}", ranking_heading(category, sort)).unwrap();
        for (path, function) in functions.into_iter().take(top) {
            render_function(&mut output, path, function);
        }
    }

    if report.summary.files == 0 && report.coverage.discovered_files == 0 {
        output.push('\n');
        writeln!(output, "No source files found.").unwrap();
    } else if !has_selected_functions {
        output.push('\n');
        writeln!(
            output,
            "No {} callables found in the analyzed {} files.",
            if all {
                "production or test"
            } else if tests {
                "test"
            } else {
                "production"
            },
            language_header(report)
        )
        .unwrap();
    }

    if !report.errors.is_empty() {
        output.push('\n');
        writeln!(output, "Analysis errors").unwrap();
        for error in &report.errors {
            let path = error.path.as_deref().unwrap_or("input");
            writeln!(
                output,
                "  {} [{}] {}",
                path,
                error_kind_name(&error.kind),
                error.message
            )
            .unwrap();
        }
    }

    output
}

fn effective_language_counts(report: &Report) -> LanguageCounts {
    if report.summary.languages.rust != 0 || report.summary.languages.python != 0 {
        return report.summary.languages.clone();
    }
    let mut counts = LanguageCounts::default();
    for file in &report.files {
        match file.language {
            Language::Rust => counts.rust = counts.rust.saturating_add(1),
            Language::Python => counts.python = counts.python.saturating_add(1),
        }
    }
    counts
}

fn language_header(report: &Report) -> &'static str {
    let languages = effective_language_counts(report);
    match (languages.rust > 0, languages.python > 0) {
        (true, false) => "Rust",
        (false, true) => "Python",
        _ => "Mixed-language",
    }
}

fn compare_functions(
    sort: SortBy,
    left_path: &str,
    left: &FunctionReport,
    right_path: &str,
    right: &FunctionReport,
) -> Ordering {
    let primary = match sort {
        SortBy::Score => right.score.value.cmp(&left.score.value),
        SortBy::Depth => right.metrics.max_depth.cmp(&left.metrics.max_depth),
        SortBy::Size => right.tokens.cmp(&left.tokens),
    };
    primary
        .then_with(|| left_path.cmp(right_path))
        .then_with(|| left.location.start.line.cmp(&right.location.start.line))
        .then_with(|| left.location.start.column.cmp(&right.location.start.column))
        .then_with(|| left.name.cmp(&right.name))
}

fn ranking_heading(category: Category, sort: SortBy) -> String {
    if sort == SortBy::Score {
        format!(
            "Most complex {} callables · sorted by {}",
            category_label(category),
            sort.label()
        )
    } else {
        format!(
            "{} callables · sorted by {}",
            category_label(category),
            sort.label()
        )
    }
}

fn render_category_summary(output: &mut String, label: &str, summary: &CategorySummary) {
    writeln!(
        output,
        "{label} · {} callables · total burden {} · average {} · p95 {} · highest {}",
        summary.functions,
        format_score(summary.total_score),
        format_average(summary.average_score),
        format_score(summary.p95_score),
        format_score(summary.highest_score),
    )
    .unwrap();
}

fn render_file_burdens(output: &mut String, report: &Report, top: usize) {
    if top == 0 || report.files.is_empty() {
        return;
    }
    let mut files = report.files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| {
        right
            .burden
            .total
            .cmp(&left.burden.total)
            .then_with(|| left.path.cmp(&right.path))
    });
    output.push('\n');
    writeln!(output, "Files by burden · top {top}").unwrap();
    for file in files.into_iter().take(top) {
        let callable_count = file.functions.len();
        let highest_callable = file
            .functions
            .iter()
            .map(|function| function.score.value)
            .max()
            .unwrap_or(0);
        writeln!(
            output,
            "  {:>5}  {} callables · highest {} · {} production · {} tests · {} · {}",
            format_score(file.burden.total),
            callable_count,
            format_score(highest_callable),
            format_score(file.burden.production),
            format_score(file.burden.test),
            file.language.label(),
            file.path
        )
        .unwrap();
    }
}

fn functions_for_category(report: &Report, category: Category) -> Vec<(&str, &FunctionReport)> {
    report
        .files
        .iter()
        .flat_map(|file| {
            file.functions
                .iter()
                .filter(move |function| function.category == category)
                .map(move |function| (file.path.as_str(), function))
        })
        .collect()
}

fn category_label(category: Category) -> &'static str {
    match category {
        Category::Production => "production",
        Category::Test => "test",
    }
}

fn render_function(output: &mut String, path: &str, function: &FunctionReport) {
    writeln!(
        output,
        "  {:>5}  {}:{}:{}  {}",
        format_score(function.score.value),
        path,
        function.location.start.line,
        function.location.start.column,
        function.name
    )
    .unwrap();
    writeln!(
        output,
        "       {} lines · {} code lines · {} Tokens · {} decisions · max depth {} · {} nesting penalty · {} statements · {} macro calls",
        function.lines,
        function.metrics.code_lines,
        function.tokens,
        function.metrics.decisions,
        function.metrics.max_depth,
        function.metrics.nesting_penalty,
        function.metrics.statements,
        function.metrics.macro_calls
    )
    .unwrap();
    writeln!(
        output,
        "       score: {} = {} boundary + {} control decisions + {} nesting + {} boolean operators + {} expression operations + {} call sites + {} explicit parameters + {} match arms",
        format_score(function.score.value),
        format_score(function.score.boundary),
        format_score(function.score.control_decisions.saturating_mul(10)),
        format_score(function.score.nesting_penalty.saturating_mul(10)),
        format_score(function.score.boolean_operator_units),
        format_score(function.score.expression_operation_units),
        format_score(function.score.call_site_units),
        format_score(function.score.parameter_units),
        format_score(function.score.match_arm_units),
    )
    .unwrap();
}

fn format_score(units: usize) -> String {
    format!("{:.1}", units as f64 / 10.0)
}

fn format_average(average: f64) -> String {
    format!("{:.1}", average / 10.0)
}

fn error_kind_name(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Read => "read",
        ErrorKind::Parse => "parse",
        ErrorKind::Lex => "lex",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AnalysisContract, Burden, Coverage, FileReport, FunctionKind, Language, Location,
        MacroOpacity, Metrics, Position, SCORE_MODEL, Score, Summary,
    };

    #[test]
    fn text_output_explains_score_and_token_count() {
        let report = Report {
            tool: "kompass".to_owned(),
            version: "0.1.0".to_owned(),
            schema_version: crate::identity::REPORT_SCHEMA_VERSION.to_owned(),
            evidence_version: crate::identity::EVIDENCE_VERSION.to_owned(),
            model: SCORE_MODEL.to_owned(),
            analysis_contract: AnalysisContract::current(),
            root: "/tmp/project".to_owned(),
            summary: Summary {
                files: 1,
                code_lines: 4,
                tokens: 12,
                languages: Default::default(),
                production: CategorySummary {
                    functions: 1,
                    total_score: 50,
                    average_score: 50.0,
                    p95_score: 50,
                    highest_score: 50,
                },
                test: CategorySummary::default(),
                burden: Burden {
                    production: 50,
                    total: 50,
                    ..Burden::default()
                },
            },
            coverage: Coverage {
                discovered_files: 1,
                analyzed_files: 1,
                complete: true,
                ..Coverage::default()
            },
            macro_opacity: MacroOpacity::default(),
            files: vec![FileReport {
                path: "src/lib.rs".to_owned(),
                language: Language::Rust,
                lines: Default::default(),
                tokens: 12,
                functions: vec![FunctionReport {
                    snapshot_id: "test-snapshot".to_owned(),
                    declaration_fingerprint: "test-declaration".to_owned(),
                    body_fingerprint: "test-body".to_owned(),
                    name: "run".to_owned(),
                    kind: FunctionKind::Function,
                    category: Category::Production,
                    location: Location {
                        start: Position { line: 1, column: 1 },
                        end: Position { line: 4, column: 2 },
                    },
                    lines: 4,
                    tokens: 12,
                    metrics: Metrics {
                        code_lines: 4,
                        decisions: 2,
                        control_decisions: 2,
                        nesting_penalty: 2,
                        max_depth: 1,
                        macro_calls: 0,
                        branches: 2,
                        ..Metrics::default()
                    },
                    score: Score {
                        value: 50,
                        units: 50,
                        display: "5.0".to_owned(),
                        decisions: 2,
                        control_decisions: 2,
                        nesting_penalty: 2,
                        boundary: 10,
                        ..Score::default()
                    },
                }],
                burden: Burden {
                    production: 50,
                    total: 50,
                    ..Burden::default()
                },
                macro_opacity: MacroOpacity::default(),
            }],
            errors: Vec::new(),
        };

        let text =
            render_report(&report, OutputFormat::Text, 10, false, false, SortBy::Score).unwrap();
        assert!(text.contains("Most complex production callables"));
        assert!(text.contains("sorted by score"));
        assert!(text.contains("12 Tokens"));
        assert!(text.contains("score: 5.0 ="));

        let json =
            render_report(&report, OutputFormat::Json, 0, false, false, SortBy::Size).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["files"][0]["functions"][0]["tokens"], 12);

        assert!(text.contains("Repository burden · 5.0 total · 5.0 production · 0.0 tests"));
    }

    #[test]
    fn text_rankings_use_the_selected_key_and_deterministic_ties() {
        let function = |name: &str, score: usize, depth: usize, tokens: usize| FunctionReport {
            snapshot_id: format!("test-{name}"),
            declaration_fingerprint: format!("declaration-{name}"),
            body_fingerprint: format!("body-{name}"),
            name: name.to_owned(),
            kind: FunctionKind::Function,
            category: Category::Production,
            location: Location {
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            },
            lines: 1,
            tokens,
            metrics: Metrics {
                max_depth: depth,
                ..Metrics::default()
            },
            score: Score {
                value: score,
                decisions: score,
                ..Score::default()
            },
        };
        let report = Report {
            tool: "kompass".to_owned(),
            version: "0.1.0".to_owned(),
            schema_version: crate::identity::REPORT_SCHEMA_VERSION.to_owned(),
            evidence_version: crate::identity::EVIDENCE_VERSION.to_owned(),
            model: SCORE_MODEL.to_owned(),
            analysis_contract: AnalysisContract::current(),
            root: "/tmp/project".to_owned(),
            summary: Summary {
                files: 2,
                production: CategorySummary {
                    functions: 3,
                    ..CategorySummary::default()
                },
                ..Summary::default()
            },
            coverage: Coverage {
                discovered_files: 2,
                analyzed_files: 2,
                complete: true,
                ..Coverage::default()
            },
            macro_opacity: MacroOpacity::default(),
            files: vec![
                FileReport {
                    path: "z.rs".to_owned(),
                    language: Language::Rust,
                    lines: Default::default(),
                    tokens: 20,
                    functions: vec![function("zeta", 10, 2, 20)],
                    burden: Default::default(),
                    macro_opacity: MacroOpacity::default(),
                },
                FileReport {
                    path: "a.rs".to_owned(),
                    language: Language::Rust,
                    lines: Default::default(),
                    tokens: 30,
                    functions: vec![function("alpha", 1, 2, 30), function("beta", 5, 3, 10)],
                    burden: Default::default(),
                    macro_opacity: MacroOpacity::default(),
                },
            ],
            errors: Vec::new(),
        };

        let depth =
            render_report(&report, OutputFormat::Text, 3, false, false, SortBy::Depth).unwrap();
        assert!(depth.contains("production callables · sorted by depth"));
        assert!(depth.find("a.rs:1:1  beta") < depth.find("a.rs:1:1  alpha"));
        assert!(depth.find("a.rs:1:1  alpha") < depth.find("z.rs:1:1  zeta"));

        let size =
            render_report(&report, OutputFormat::Text, 3, false, false, SortBy::Size).unwrap();
        assert!(size.contains("production callables · sorted by size"));
        assert!(size.find("a.rs:1:1  alpha") < size.find("z.rs:1:1  zeta"));
        assert!(size.find("z.rs:1:1  zeta") < size.find("a.rs:1:1  beta"));
    }
}
