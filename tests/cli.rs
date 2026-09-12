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
fn default_model_matches_explicit_v3_and_old_models_remain_selectable() {
    let root = temporary_directory("default-model");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("main.rs"),
        "fn sample(value: i32) { let _ = value + 1; }\n",
    )
    .unwrap();

    let run = |model: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kompass"));
        if let Some(model) = model {
            command.args(["--model", model]);
        }
        command.args(["--format", "json", root.to_str().unwrap()]);
        command.output().unwrap()
    };

    let implicit = run(None);
    let explicit_v3 = run(Some("structural-v3"));
    assert!(implicit.status.success());
    assert!(explicit_v3.status.success());
    let implicit_report: serde_json::Value = serde_json::from_slice(&implicit.stdout).unwrap();
    let explicit_report: serde_json::Value = serde_json::from_slice(&explicit_v3.stdout).unwrap();
    assert_eq!(implicit_report, explicit_report);

    for model in ["structural-v1", "structural-v2"] {
        let output = run(Some(model));
        assert!(output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["model"], model);
    }

    std::fs::remove_dir_all(root).unwrap();
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
fn structural_v2_is_explicit_and_keeps_integer_units_in_json() {
    let root = temporary_directory("v2");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("main.rs"),
        "fn sample(value: bool) { if value && value { println!(\"ok\"); } }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "--model",
            "structural-v2",
            "--format",
            "json",
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["model"], "structural-v2");
    let score = &report["files"][0]["functions"][0]["score"];
    assert!(score["value"].is_u64());
    assert_eq!(score["value"], score["units"]);
    assert_eq!(score["display"], "2.9");
    assert_eq!(report["files"][0]["burden"]["total"], score["value"]);
    assert_eq!(report["summary"]["burden"]["total"], score["value"]);
    assert_eq!(report["macro_opacity"]["invocations"], 1);
    assert_eq!(report["files"][0]["macro_opacity"]["invocations"], 1);
    assert!(report["macro_opacity"]["source_tokens"].as_u64().unwrap() > 0);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn structural_v3_reports_expression_operation_units_in_json() {
    let root = temporary_directory("v3-json");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("main.rs"),
        "fn sample(value: i32) { let _ = (value + 1) as i64; }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "--model",
            "structural-v3",
            "--format",
            "json",
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["model"], "structural-v3");
    let function = &report["files"][0]["functions"][0];
    assert_eq!(function["metrics"]["expression_operations"], 2);
    assert_eq!(function["score"]["expression_operation_units"], 2);
    assert_eq!(function["score"]["statement_penalty"], 0);
    assert_eq!(function["score"]["value"], 14);
    assert_eq!(function["score"]["display"], "1.4");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn structural_v3_text_shows_the_exact_operation_breakdown() {
    let root = temporary_directory("v3-text");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("main.rs"),
        "fn sample(value: i32) { let _ = (value + 1) as i64; }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args(["--model", "structural-v3", root.to_str().unwrap()])
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
