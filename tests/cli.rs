use std::process::Command;

fn temporary_directory(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "kompass-cli-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn help_explains_agent_workflow_and_json_contract() {
    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);

    for expected in [
        "kompass --format json PATH > before.json",
        "kompass --format json PATH > after.json",
        "behavior-preserving refactor",
        "score comparison cannot",
        "summary.burden.production",
        "integer tenths",
        "files[].burden",
        "files[].functions[].score.value",
        "files[].functions[].metrics",
        "macro_opacity.invocations",
        "macro_opacity.source_tokens",
        "without expansion, including built-in macros",
        "Production and test",
        "scored and summarized separately",
        "--top",
        "--sort",
        "text output only",
        "top-level `model` values match",
        "coverage.complete",
        "errors",
        "status 0",
        "status 2",
        "partial JSON report",
        "lower score is a review signal",
        "module boundaries and coupling",
        "do not blindly minimize the score",
    ] {
        assert!(
            help.contains(expected),
            "help is missing {expected:?}\n{help}"
        );
    }
    assert!(
        !help.contains("--model"),
        "legacy model selection remains in help\n{help}"
    );
}

#[test]
fn short_help_points_to_the_workflow() {
    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .arg("-h")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("Use `kompass --help`"));
}

#[test]
fn json_report_contains_separate_categories_and_tokens() {
    let root = temporary_directory("json");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("main.rs"),
        "fn production(value: bool) { if value {} }\n#[test]\nfn test_case() {}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--format", "json", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["model"], "structural-v3");
    assert_eq!(report["summary"]["production"]["functions"], 1);
    assert_eq!(report["summary"]["test"]["functions"], 1);
    assert_eq!(report["files"][0]["functions"][0]["category"], "production");
    assert!(
        report["files"][0]["functions"][0]["tokens"]
            .as_u64()
            .unwrap()
            > 0
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_model_selection_is_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--model", "structural-v1"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unexpected argument '--model'"), "{stderr}");
}

#[test]
fn tests_and_all_are_rejected_by_the_cli() {
    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--tests", "--all"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot be used with"));
}

#[test]
fn sort_options_are_accepted_and_visible_in_text() {
    let root = temporary_directory("sort");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("main.rs"),
        "fn shallow() {}\nfn deep(value: bool) { if value { if value {} } }\n",
    )
    .unwrap();

    for (sort, heading) in [
        ("score", "sorted by score"),
        ("depth", "sorted by depth"),
        ("size", "sorted by size"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
            .args(["--sort", sort, root.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains(heading));
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reports_expression_operation_units_in_json() {
    let root = temporary_directory("score-json");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("main.rs"),
        "fn sample(value: i32) { let _ = (value + 1) as i64; }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--format", "json", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["model"], "structural-v3");
    let function = &report["files"][0]["functions"][0];
    assert_eq!(function["metrics"]["expression_operations"], 2);
    assert_eq!(function["score"]["expression_operation_units"], 2);
    assert_eq!(function["score"]["value"], 14);
    assert_eq!(function["score"]["display"], "1.4");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn text_shows_the_exact_operation_breakdown() {
    let root = temporary_directory("score-text");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("main.rs"),
        "fn sample(value: i32) { let _ = (value + 1) as i64; }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .arg(root.to_str().unwrap())
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Kompass 0.1.0 · Rust structural complexity · structural-v3"));
    assert!(text.contains(
        "score: 1.4 = 1.0 boundary + 0.0 control decisions + 0.0 nesting + 0.0 boolean operators + 0.2 expression operations + 0.0 call sites + 0.2 explicit parameters + 0.0 match arms"
    ));

    std::fs::remove_dir_all(root).unwrap();
}
