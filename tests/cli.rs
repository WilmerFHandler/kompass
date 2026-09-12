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
        "kompass diff before.json after.json",
        "behavior-preserving refactor",
        "score comparison cannot",
        "summary.burden.production",
        "integer tenths",
        "files[].burden",
        "files[].functions[].score.value",
        "files[].functions[].metrics",
        "macro_opacity.invocations",
        "macro_opacity.source_tokens",
        "macro_opacity.definitions",
        "macro_opacity.definition_tokens",
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
    assert_eq!(report["model"], "structural-v4");
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
    assert_eq!(report["model"], "structural-v4");
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
    assert!(text.contains("Kompass 0.1.0 · Rust structural complexity · structural-v4"));
    assert!(text.contains(
        "score: 1.4 = 1.0 boundary + 0.0 control decisions + 0.0 nesting + 0.0 boolean operators + 0.2 expression operations + 0.0 call sites + 0.2 explicit parameters + 0.0 match arms"
    ));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn diff_reports_show_function_groups_burden_components_and_opacity() {
    let root = temporary_directory("diff");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("main.rs");
    let before_path = root.join("before.json");
    let after_path = root.join("after.json");
    std::fs::write(
        &source,
        "fn stable(value: bool) { if value {} }\nfn removed() {}\n",
    )
    .unwrap();

    let before = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--format", "json", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(before.status.success());
    std::fs::write(&before_path, &before.stdout).unwrap();

    std::fs::write(
        &source,
        "fn stable(value: bool) { if value { if value {} } }\nfn added() {}\n",
    )
    .unwrap();
    let after = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--format", "json", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(after.status.success());
    std::fs::write(&after_path, &after.stdout).unwrap();

    let comparison = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "diff",
            before_path.to_str().unwrap(),
            after_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(comparison.status.success());
    let text = String::from_utf8_lossy(&comparison.stdout);
    for expected in [
        "Production burden",
        "Changed callables · 1",
        "Added callables · 1",
        "Removed callables · 1",
        "stable",
        "added",
        "removed",
        "Top score component deltas",
        "Macro opacity",
        "Callable summary deltas",
        "File burden deltas",
    ] {
        assert!(
            text.contains(expected),
            "diff is missing {expected:?}\n{text}"
        );
    }

    let json = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "diff",
            "--format",
            "json",
            before_path.to_str().unwrap(),
            after_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(json.status.success());
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["comparable"], true);
    assert_eq!(report["functions"]["changed"].as_array().unwrap().len(), 1);
    assert_eq!(report["functions"]["added"].as_array().unwrap().len(), 1);
    assert_eq!(report["functions"]["removed"].as_array().unwrap().len(), 1);
    assert!(report["callables"]["production"]["p95_score"].is_object());
    assert_eq!(report["file_burdens"].as_array().unwrap().len(), 1);
    assert_eq!(report["macro_opacity"]["invocations"]["delta"], 0);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn diff_rejects_model_and_coverage_mismatches() {
    let root = temporary_directory("diff-validation");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.rs"), "fn stable() {}\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--format", "json", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let original_model = report["model"].clone();

    let before_path = root.join("before.json");
    let model_mismatch_path = root.join("model-mismatch.json");
    std::fs::write(&before_path, &output.stdout).unwrap();
    report["model"] = serde_json::Value::String("other-model".to_owned());
    std::fs::write(
        &model_mismatch_path,
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    let mismatch = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "diff",
            before_path.to_str().unwrap(),
            model_mismatch_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(mismatch.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&mismatch.stderr);
    assert!(stderr.contains("score model differs"), "{stderr}");

    report["model"] = original_model;
    report["coverage"]["complete"] = serde_json::Value::Bool(false);
    let incomplete_path = root.join("incomplete.json");
    std::fs::write(
        &incomplete_path,
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    let incomplete = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "diff",
            before_path.to_str().unwrap(),
            incomplete_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(incomplete.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&incomplete.stderr);
    assert!(stderr.contains("after coverage is incomplete"), "{stderr}");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn diff_requires_opt_in_for_file_changes_and_highlights_scope_delta() {
    let root = temporary_directory("diff-file-scope");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.rs"), "fn stable() {}\n").unwrap();

    let before = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--format", "json", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(before.status.success());
    let before_path = root.join("before.json");
    std::fs::write(&before_path, &before.stdout).unwrap();

    std::fs::write(root.join("new.rs"), "fn added_file() {}\n").unwrap();
    let after = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--format", "json", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(after.status.success());
    let after_path = root.join("after.json");
    std::fs::write(&after_path, &after.stdout).unwrap();

    let strict = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "diff",
            before_path.to_str().unwrap(),
            after_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&strict.stderr);
    assert!(stderr.contains("file scope differs"), "{stderr}");
    assert!(stderr.contains("--allow-file-changes"), "{stderr}");

    let allowed = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "diff",
            "--allow-file-changes",
            before_path.to_str().unwrap(),
            after_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(allowed.status.success());
    let text = String::from_utf8_lossy(&allowed.stdout);
    for expected in [
        "WARNING: file set changed",
        "Added files · 1",
        "new.rs",
        "aggregate burden and component deltas include added and removed files",
    ] {
        assert!(
            text.contains(expected),
            "diff is missing {expected:?}\n{text}"
        );
    }

    std::fs::remove_dir_all(root).unwrap();
}
