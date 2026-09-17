//! Acceptance and calibration probes for the JavaScript/TypeScript frontend.
//!
//! The tests intentionally exercise the CLI JSON boundary instead of calling
//! parser internals. This keeps the corpus tied to the same discovery,
//! analysis, identity, evidence, explain, and diff contracts that users see.
//! They are ignored on the structural-v4 Rust/Python baseline and should be
//! enabled when the JavaScript/TypeScript frontend lands.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

fn temporary_directory(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kompass-javascript-{label}-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn materialize_fixture(root: &Path, fixture: &str, destination: &str) -> PathBuf {
    let source_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/javascript")
        .join(fixture);
    let source = fs::read_to_string(&source_path)
        .unwrap_or_else(|error| panic!("cannot read fixture {}: {error}", source_path.display()));
    let destination = root.join(destination);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&destination, source).unwrap();
    destination
}

fn run_json(path: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kompass"));
    command.arg("--format").arg("json").args(args).arg(path);
    command.output().unwrap()
}

fn run_json_report(path: &Path, args: &[&str]) -> (Output, Value) {
    let output = run_json(path, args);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Kompass did not emit JSON (status {:?}): {error}\nstderr:\n{}\nstdout:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        )
    });
    (output, report)
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "Kompass failed with {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn function_values(report: &Value) -> Vec<&Value> {
    report["files"]
        .as_array()
        .unwrap_or_else(|| panic!("report has no files array: {report}"))
        .iter()
        .flat_map(|file| {
            file["functions"]
                .as_array()
                .unwrap_or_else(|| panic!("file has no functions array: {file}"))
                .iter()
        })
        .collect()
}

fn function_named<'a>(report: &'a Value, name: &str) -> &'a Value {
    function_values(report)
        .into_iter()
        .find(|function| function["name"] == name)
        .unwrap_or_else(|| panic!("missing callable {name:?} in {report}"))
}

fn assert_contract(report: &Value) {
    assert_eq!(report["model"], "structural-v4");
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["evidence_version"], "evidence-v1");
    assert_eq!(report["evidence"]["contract"], "evidence-v1");
    assert_eq!(report["analysis_contract"]["version"], "analysis-v1");
    assert!(
        report["analysis_contract"]["frontend"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(
        report["scope"]["category_policy"],
        "production-test-separated-v1"
    );
}

fn assert_score_reconciles(function: &Value) {
    let expected = function["score"]["units"]
        .as_u64()
        .unwrap_or_else(|| panic!("score has no integer units: {function}"));
    let actual = function["score"]["contributions"]
        .as_array()
        .unwrap_or_else(|| panic!("score has no contributions: {function}"))
        .iter()
        .map(|contribution| contribution["units"].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(
        actual, expected,
        "score contributions do not reconcile for {}",
        function["name"]
    );
}

fn assert_metric_fields_equal(before: &Value, after: &Value) {
    for field in [
        "statements",
        "expression_operations",
        "decisions",
        "control_decisions",
        "nesting_penalty",
        "max_depth",
        "branches",
        "boolean_operators",
        "match_arms",
        "loops",
        "returns",
        "mutations",
        "parameters",
        "explicit_parameters",
        "call_sites",
        "closures",
        "unsafe_blocks",
    ] {
        assert_eq!(
            before["metrics"][field], after["metrics"][field],
            "metric {field} changed after type erasure"
        );
    }
}

fn assert_score_values_equal(before: &Value, after: &Value) {
    for field in [
        "value",
        "units",
        "boundary",
        "control_decisions",
        "nesting_penalty",
        "boolean_operator_units",
        "call_site_units",
        "parameter_units",
        "match_arm_units",
        "expression_operation_units",
    ] {
        assert_eq!(
            before["score"][field], after["score"][field],
            "score component {field} changed after type erasure"
        );
    }
}

#[test]
fn fixture_corpus_covers_the_requested_surfaces() {
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/javascript");
    for fixture in [
        "react_surface.jsx",
        "react_native.tsx",
        "runtime.js",
        "runtime.ts",
        "control_flow.js",
        "initializers.ts",
        "anonymous_default.tsx",
        "duplicates.js",
        "callback_base.js",
        "callback_nested.js",
        "callback_depth.js",
        "diff_before.tsx",
        "diff_after.tsx",
        "malformed.tsx",
        "exclusions/src/keep.js",
        "exclusions/src/native.tsx",
        "exclusions/tests/widget.test.jsx",
        "exclusions/__tests__/screen.spec.tsx",
        "exclusions/node_modules/pkg/index.js",
        "exclusions/dist/generated.js",
        "exclusions/coverage/report.js",
        "exclusions/.expo/state.js",
        "exclusions/build/bundle.js",
        "exclusions/src/api.generated.ts",
        "exclusions/src/app.min.js",
        "exclusions/src/app.bundle.js",
        "exclusions/types.d.ts",
    ] {
        assert!(
            fixture_root.join(fixture).is_file(),
            "missing fixture {fixture}"
        );
    }

    let surface = fs::read_to_string(fixture_root.join("react_surface.jsx")).unwrap();
    for syntax in [
        "useEffect",
        "useMemo",
        "onPress",
        ".map(",
        "?.",
        "??",
        "? <",
    ] {
        assert!(
            surface.contains(syntax),
            "surface fixture is missing {syntax:?}"
        );
    }
    let native = fs::read_to_string(fixture_root.join("react_native.tsx")).unwrap();
    for syntax in ["type ", "switch", "catch", "<View>", " as "] {
        assert!(
            native.contains(syntax),
            "native fixture is missing {syntax:?}"
        );
    }
    let initializers = fs::read_to_string(fixture_root.join("initializers.ts")).unwrap();
    assert!(initializers.contains("satisfies"));
}

#[test]
fn all_four_extensions_are_discovered_and_scored() {
    let root = temporary_directory("extensions");
    fs::create_dir_all(&root).unwrap();
    for (fixture, destination) in [
        ("react_surface.jsx", "src/react_surface.jsx"),
        ("react_native.tsx", "src/react_native.tsx"),
        ("runtime.js", "src/runtime.js"),
        ("runtime.ts", "src/runtime.ts"),
    ] {
        materialize_fixture(&root, fixture, destination);
    }

    let (output, report) = run_json_report(&root, &[]);
    assert_success(&output);
    assert_contract(&report);
    assert_eq!(report["scope"]["language_filter"], "all");
    assert_eq!(report["coverage"]["complete"], true);
    assert_eq!(report["summary"]["files"], 4);
    assert_eq!(report["summary"]["languages"]["javascript"], 2);
    assert_eq!(report["summary"]["languages"]["typescript"], 2);
    assert_eq!(report["summary"]["by_language"]["javascript"]["files"], 2);
    assert_eq!(report["summary"]["by_language"]["typescript"]["files"], 2);
    assert_eq!(report["coverage"]["test_files"], 0);
    assert!(!function_values(&report).is_empty());

    let mut ids = BTreeSet::new();
    for function in function_values(&report) {
        assert!(function["tokens"].as_u64().unwrap() > 0);
        assert!(
            function["snapshot_id"]
                .as_str()
                .is_some_and(|value| value.starts_with("identity-v1:snapshot:"))
        );
        assert!(ids.insert(function["snapshot_id"].as_str().unwrap().to_owned()));
        assert_score_reconciles(function);
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn react_components_hooks_and_jsx_callbacks_are_exclusive_units() {
    let root = temporary_directory("react-surface");
    fs::create_dir_all(&root).unwrap();
    let path = materialize_fixture(&root, "react_surface.jsx", "src/ListScreen.jsx");
    let (output, report) = run_json_report(&path, &[]);
    assert_success(&output);
    assert_contract(&report);

    let component = function_named(&report, "ListScreen");
    assert_eq!(component["kind"], "function");
    assert!(component["metrics"]["decisions"].as_u64().unwrap() >= 2);
    let hook = function_named(&report, "useVisibleItems");
    assert!(matches!(
        hook["kind"].as_str(),
        Some("closure" | "lambda" | "function")
    ));
    assert!(hook["metrics"]["call_sites"].as_u64().unwrap() > 0);
    let render_item = function_named(&report, "ListScreen::renderItem");
    assert!(
        render_item["metrics"]["control_decisions"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(component["metrics"]["closures"].as_u64().unwrap() >= 1);

    let callbacks = function_values(&report)
        .into_iter()
        .filter(|function| matches!(function["kind"].as_str(), Some("closure" | "lambda")))
        .collect::<Vec<_>>();
    // useMemo, filter/map callbacks, useEffect, renderItem, the press handler,
    // and keyExtractor all live in this small React surface.
    assert!(
        callbacks.len() >= 6,
        "expected JSX and hook callbacks, got {callbacks:?}"
    );

    let mut ids = BTreeSet::new();
    for function in function_values(&report) {
        assert_score_reconciles(function);
        assert!(ids.insert(function["snapshot_id"].as_str().unwrap().to_owned()));
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn nested_callback_bodies_do_not_leak_into_outer_scores() {
    let base_root = temporary_directory("callback-base");
    let nested_root = temporary_directory("callback-nested");
    fs::create_dir_all(&base_root).unwrap();
    fs::create_dir_all(&nested_root).unwrap();
    let base_path = materialize_fixture(&base_root, "callback_base.js", "callback.js");
    let nested_path = materialize_fixture(&nested_root, "callback_nested.js", "callback.js");

    let (base_output, base_report) = run_json_report(&base_path, &[]);
    let (nested_output, nested_report) = run_json_report(&nested_path, &[]);
    assert_success(&base_output);
    assert_success(&nested_output);
    let base_outer = function_named(&base_report, "callbackOwners");
    let nested_outer = function_named(&nested_report, "callbackOwners");
    assert_metric_fields_equal(base_outer, nested_outer);
    assert_score_values_equal(base_outer, nested_outer);
    assert!(
        nested_report["summary"]["burden"]["total"]
            .as_u64()
            .unwrap()
            > base_report["summary"]["burden"]["total"].as_u64().unwrap(),
        "nested callback complexity must remain visible in aggregate burden"
    );

    let nested_callback = function_values(&nested_report)
        .into_iter()
        .find(|function| {
            matches!(function["kind"].as_str(), Some("closure" | "lambda"))
                && function["metrics"]["control_decisions"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0
        })
        .expect("nested callback body was not reported as a callable");
    assert!(nested_callback["score"]["units"].as_u64().unwrap() > 10);
    assert_score_reconciles(nested_callback);

    let depth_root = temporary_directory("callback-depth");
    fs::create_dir_all(&depth_root).unwrap();
    let depth_path = materialize_fixture(&depth_root, "callback_depth.js", "depth.js");
    let (depth_output, depth_report) = run_json_report(&depth_path, &[]);
    assert_success(&depth_output);
    let depth_callback = function_values(&depth_report)
        .into_iter()
        .find(|function| matches!(function["kind"].as_str(), Some("closure" | "lambda")))
        .expect("depth callback was not reported");
    assert!(depth_callback["metrics"]["max_depth"].as_u64().unwrap() > 0);
    assert!(
        depth_callback["metrics"]["nesting_penalty"]
            .as_u64()
            .unwrap()
            > 0
    );

    fs::remove_dir_all(base_root).unwrap();
    fs::remove_dir_all(nested_root).unwrap();
    fs::remove_dir_all(depth_root).unwrap();
}

#[test]
fn modern_js_control_flow_remains_visible_to_structural_v4() {
    let root = temporary_directory("control-flow");
    fs::create_dir_all(&root).unwrap();
    let path = materialize_fixture(&root, "control_flow.js", "control_flow.js");
    let (output, report) = run_json_report(&path, &[]);
    assert_success(&output);
    assert_contract(&report);

    let plain = function_named(&report, "plain");
    for name in ["optional", "nullish", "ternary", "switching", "catching"] {
        let function = function_named(&report, name);
        assert!(
            function["score"]["units"].as_u64().unwrap()
                > plain["score"]["units"].as_u64().unwrap(),
            "{name} must expose a structural signal beyond a plain return"
        );
        assert_score_reconciles(function);
    }
    assert!(
        function_named(&report, "optional")["metrics"]["decisions"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        function_named(&report, "nullish")["metrics"]["decisions"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        function_named(&report, "ternary")["metrics"]["control_decisions"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        function_named(&report, "switching")["metrics"]["match_arms"]
            .as_u64()
            .unwrap()
            >= 3
    );
    assert!(
        function_named(&report, "catching")["metrics"]["control_decisions"]
            .as_u64()
            .unwrap()
            > 0
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn module_and_class_initializers_are_reported_once() {
    let root = temporary_directory("initializers");
    fs::create_dir_all(&root).unwrap();
    let path = materialize_fixture(&root, "initializers.ts", "initializers.ts");
    let (output, report) = run_json_report(&path, &[]);
    assert_success(&output);
    assert_contract(&report);

    let initializers = function_values(&report)
        .into_iter()
        .filter(|function| {
            matches!(
                function["kind"].as_str(),
                Some("module_initializer" | "class_initializer")
            )
        })
        .collect::<Vec<_>>();
    assert!(
        initializers.len() >= 2,
        "expected module and class field initializers, got {initializers:?}"
    );
    assert!(
        initializers
            .iter()
            .any(|function| function["kind"] == "module_initializer")
    );
    assert!(
        initializers
            .iter()
            .any(|function| function["kind"] == "class_initializer")
    );
    assert!(
        function_values(&report)
            .into_iter()
            .any(|function| function["kind"] == "method")
    );
    assert!(
        function_values(&report)
            .into_iter()
            .all(|function| function["name"] != "Config")
    );
    for function in initializers {
        assert!(function["score"]["units"].as_u64().unwrap() >= 10);
        assert_score_reconciles(function);
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn anonymous_and_default_exports_remain_callable_units() {
    let root = temporary_directory("anonymous-default");
    fs::create_dir_all(&root).unwrap();
    let path = materialize_fixture(&root, "anonymous_default.tsx", "anonymous_default.tsx");
    let (output, report) = run_json_report(&path, &[]);
    assert_success(&output);
    assert_contract(&report);
    let functions = function_values(&report);
    assert!(functions.len() >= 2);
    assert!(functions.iter().any(|function| function["name"] == "named"));
    let default_unit = functions
        .iter()
        .find(|function| function["location"]["start"]["line"] == 3)
        .expect("default export arrow was not reported");
    assert!(
        default_unit["name"]
            .as_str()
            .is_some_and(|name| !name.is_empty())
    );
    assert!(matches!(
        default_unit["kind"].as_str(),
        Some("function" | "closure" | "lambda")
    ));
    assert!(functions.iter().all(|function| function["name"] != "Props"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn type_annotations_and_assertions_are_score_invariant() {
    let js_root = temporary_directory("type-erasure-js");
    let ts_root = temporary_directory("type-erasure-ts");
    fs::create_dir_all(&js_root).unwrap();
    fs::create_dir_all(&ts_root).unwrap();
    let js_path = materialize_fixture(&js_root, "runtime.js", "runtime.js");
    let ts_path = materialize_fixture(&ts_root, "runtime.ts", "runtime.ts");
    let (js_output, js_report) = run_json_report(&js_path, &[]);
    let (ts_output, ts_report) = run_json_report(&ts_path, &[]);
    assert_success(&js_output);
    assert_success(&ts_output);
    assert_contract(&js_report);
    assert_contract(&ts_report);

    for name in ["normalize", "run"] {
        let js_function = function_named(&js_report, name);
        let ts_function = function_named(&ts_report, name);
        assert_metric_fields_equal(js_function, ts_function);
        assert_score_values_equal(js_function, ts_function);
        assert_score_reconciles(js_function);
        assert_score_reconciles(ts_function);
    }
    assert!(
        function_values(&ts_report)
            .into_iter()
            .all(|function| function["name"] != "Item")
    );

    fs::remove_dir_all(js_root).unwrap();
    fs::remove_dir_all(ts_root).unwrap();
}

#[test]
fn malformed_typescript_is_a_partial_report_with_valid_siblings() {
    let root = temporary_directory("malformed");
    fs::create_dir_all(&root).unwrap();
    materialize_fixture(&root, "react_surface.jsx", "src/valid.jsx");
    materialize_fixture(&root, "malformed.tsx", "src/malformed.tsx");
    let (output, report) = run_json_report(&root, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert_contract(&report);
    assert_eq!(report["coverage"]["discovered_files"], 2);
    assert_eq!(report["coverage"]["analyzed_files"], 1);
    assert_eq!(report["coverage"]["failed_files"], 1);
    assert_eq!(report["coverage"]["complete"], false);
    assert!(
        report["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "src/valid.jsx")
    );
    let error = report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|error| error["path"] == "src/malformed.tsx")
        .expect("malformed source error is missing");
    assert_eq!(error["kind"], "parse");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn mixed_scope_classifies_tests_and_excludes_generated_dependencies() {
    let root = temporary_directory("scope");
    fs::create_dir_all(&root).unwrap();
    for (fixture, destination) in [
        ("exclusions/src/keep.js", "src/keep.js"),
        ("exclusions/src/native.tsx", "src/native.tsx"),
        ("exclusions/tests/widget.test.jsx", "tests/widget.test.jsx"),
        (
            "exclusions/__tests__/screen.spec.tsx",
            "__tests__/screen.spec.tsx",
        ),
        (
            "exclusions/node_modules/pkg/index.js",
            "node_modules/pkg/index.js",
        ),
        ("exclusions/dist/generated.js", "dist/generated.js"),
        ("exclusions/coverage/report.js", "coverage/report.js"),
        ("exclusions/.expo/state.js", ".expo/state.js"),
        ("exclusions/build/bundle.js", "build/bundle.js"),
        ("exclusions/src/api.generated.ts", "src/api.generated.ts"),
        ("exclusions/src/app.min.js", "src/app.min.js"),
        ("exclusions/src/app.bundle.js", "src/app.bundle.js"),
        ("exclusions/types.d.ts", "types.d.ts"),
    ] {
        materialize_fixture(&root, fixture, destination);
    }

    let (output, report) = run_json_report(&root, &[]);
    assert_success(&output);
    assert_contract(&report);
    assert_eq!(report["coverage"]["complete"], true);
    assert_eq!(report["summary"]["files"], 4);
    assert_eq!(report["summary"]["languages"]["javascript"], 2);
    assert_eq!(report["summary"]["languages"]["typescript"], 2);
    assert_eq!(report["coverage"]["test_files"], 2);
    assert_eq!(report["summary"]["production"]["functions"], 2);
    assert_eq!(report["summary"]["test"]["functions"], 2);
    for file in report["files"].as_array().unwrap() {
        let path = file["path"].as_str().unwrap();
        assert!(!path.contains("node_modules"));
        assert!(!path.starts_with("dist/"));
        assert!(!path.starts_with("coverage/"));
        assert!(!path.starts_with(".expo/"));
        assert!(!path.starts_with("build/"));
    }

    for (language, expected_files) in [("javascript", 2), ("typescript", 2)] {
        let (filtered_output, filtered) = run_json_report(&root, &["--language", language]);
        assert_success(&filtered_output);
        assert_eq!(filtered["summary"]["files"], expected_files);
        assert!(
            filtered["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| { file["language"] == language })
        );
    }
    let (explicit_output, explicit) =
        run_json_report(&root.join("src/native.tsx"), &["--language", "javascript"]);
    assert_success(&explicit_output);
    assert_eq!(explicit["summary"]["files"], 1);
    assert_eq!(explicit["files"][0]["language"], "typescript");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn duplicate_names_keep_distinct_ids_and_evidence_contract() {
    let root = temporary_directory("duplicates");
    fs::create_dir_all(&root).unwrap();
    let path = materialize_fixture(&root, "duplicates.js", "duplicates.js");
    let (output, report) = run_json_report(&path, &[]);
    assert_success(&output);
    assert_contract(&report);

    let duplicate_units = function_values(&report)
        .into_iter()
        .filter(|function| {
            function["name"].as_str().is_some_and(|name| {
                name == "duplicate" || name.ends_with(".duplicate") || name.ends_with("::duplicate")
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        duplicate_units.len(),
        2,
        "same-named methods must both survive"
    );
    assert_ne!(
        duplicate_units[0]["snapshot_id"],
        duplicate_units[1]["snapshot_id"]
    );
    let groups = report["evidence"]["duplicates"]["groups"]
        .as_array()
        .unwrap();
    assert!(
        !groups.is_empty(),
        "duplicate statement evidence is missing"
    );
    assert!(groups.iter().any(|group| {
        group["language"] == "javascript"
            && group["occurrences"]
                .as_array()
                .is_some_and(|items| items.len() >= 2)
    }));
    assert!(
        report["evidence"]["call_graph"]["coverage"]["call_sites"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        report["evidence"]["call_graph"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["callee_name"]
                .as_str()
                .is_some_and(|name| name.contains("duplicate")))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explain_preserves_eight_components_for_jsx_callback_lines() {
    let root = temporary_directory("explain");
    fs::create_dir_all(&root).unwrap();
    let path = materialize_fixture(&root, "react_surface.jsx", "src/ListScreen.jsx");
    let source = fs::read_to_string(&path).unwrap();
    let line = source
        .lines()
        .position(|line| line.contains("onSelect?."))
        .map(|line| line + 1)
        .expect("fixture lost the event handler");
    let output = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "explain",
            path.to_str().unwrap(),
            "--line",
            &line.to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_success(&output);
    let explanation: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(explanation["report_kind"], "explain");
    assert_eq!(explanation["model"], "structural-v4");
    assert_eq!(explanation["evidence_version"], "evidence-v1");
    assert_eq!(explanation["evidence"]["contract"], "evidence-v1");
    let candidates = explanation["candidates"].as_array().unwrap();
    assert!(!candidates.is_empty());
    assert_eq!(
        candidates
            .iter()
            .filter(|candidate| candidate["innermost"] == true)
            .count(),
        1
    );
    for candidate in candidates {
        let components = candidate["components"].as_array().unwrap();
        assert_eq!(components.len(), 8);
        let units = components
            .iter()
            .map(|component| component["units"].as_u64().unwrap())
            .sum::<u64>();
        assert_eq!(units, candidate["score"]["units"]);
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn diff_keeps_structural_and_analysis_contracts_for_tsx_changes() {
    let root = temporary_directory("diff");
    fs::create_dir_all(&root).unwrap();
    let source_path = root.join("screen.tsx");
    fs::write(
        &source_path,
        fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/javascript/diff_before.tsx"),
        )
        .unwrap(),
    )
    .unwrap();
    let (before_output, before_report) = run_json_report(&root, &[]);
    assert_success(&before_output);
    let before_path = root.join("before.json");
    fs::write(&before_path, &before_output.stdout).unwrap();

    fs::write(
        &source_path,
        fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/javascript/diff_after.tsx"),
        )
        .unwrap(),
    )
    .unwrap();
    let (after_output, after_report) = run_json_report(&root, &[]);
    assert_success(&after_output);
    let after_path = root.join("after.json");
    fs::write(&after_path, &after_output.stdout).unwrap();
    assert_eq!(before_report["model"], "structural-v4");
    assert_eq!(after_report["model"], "structural-v4");
    assert_eq!(
        before_report["analysis_contract"],
        after_report["analysis_contract"]
    );

    let comparison = Command::new(env!("CARGO_BIN_EXE_kompass"))
        .args([
            "diff",
            "--format",
            "json",
            before_path.to_str().unwrap(),
            after_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_success(&comparison);
    let diff: Value = serde_json::from_slice(&comparison.stdout).unwrap();
    assert_eq!(diff["comparable"], true);
    assert_eq!(diff["before"]["model"], "structural-v4");
    assert_eq!(diff["after"]["model"], "structural-v4");
    assert_eq!(diff["before"]["evidence_version"], "evidence-v1");
    assert_eq!(diff["after"]["evidence_version"], "evidence-v1");
    assert_eq!(
        diff["before"]["analysis_contract"],
        before_report["analysis_contract"]
    );
    assert!(!diff["functions"]["changed"].as_array().unwrap().is_empty());
    assert!(
        diff["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|warning| { !warning.as_str().unwrap().contains("coverage") })
    );

    fs::remove_dir_all(root).unwrap();
}
