use std::path::PathBuf;

use kompass::analyze::analyze;
use kompass::discover::{DiscoveredFile, Discovery};
use kompass::model::{Category, Language};

fn report(source: &str, language: Language) -> kompass::Report {
    let root = std::env::temp_dir().join(format!(
        "kompass-evidence-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let extension = match language {
        Language::Rust => "rs",
        Language::Python => "py",
    };
    let path = root.join(format!("main.{extension}"));
    std::fs::write(&path, source).unwrap();
    let result = analyze(
        &root,
        Discovery {
            files: vec![DiscoveredFile {
                path: path.clone(),
                language,
                category: Category::Production,
            }],
            test_files: 0,
        },
    );
    remove_tree(&root);
    result
}

fn remove_tree(path: &PathBuf) {
    let _ = std::fs::remove_dir_all(path);
}

#[test]
fn rust_edges_are_conservative_and_regions_charge_once() {
    let source = r#"
use std::fmt::Debug;

fn target(value: i32) -> i32 { value + 1 }

fn caller(target: fn(i32) -> i32, value: i32) -> i32 {
    let alias = target;
    let assigned = value;
    target(value) + alias(value) + assigned
}

fn use_target(value: i32) -> i32 { target(value) }

struct Item;
impl Item { fn run(&self) -> i32 { target(1) } }
"#;
    let report = report(source, Language::Rust);
    let calls = &report.evidence.call_graph;
    assert!(calls.coverage.call_sites >= 4);
    assert!(calls.edges.iter().any(|edge| edge.callee_name == "target"
        && edge.resolution == kompass::evidence::CallResolution::Resolved));
    assert!(calls.edges.iter().any(|edge| {
        edge.callee_name == "target"
            && edge.reason == kompass::evidence::CallResolutionReason::Parameter
            && edge.resolution == kompass::evidence::CallResolution::Unresolved
    }));
    assert!(calls.edges.iter().any(|edge| {
        edge.callee_name == "alias" && edge.reason == kompass::evidence::CallResolutionReason::Alias
    }));
    let region = calls
        .regions
        .iter()
        .find(|region| region.root.name.ends_with("use_target"))
        .unwrap();
    assert_eq!(region.unique_units, 2);
    assert!(
        region
            .reachable
            .iter()
            .any(|unit| unit.name.ends_with("target"))
    );
}

#[test]
fn python_edges_mark_parameters_assignments_aliases_and_methods() {
    let source = r#"
from math import sqrt as root

def target(value):
    return value + 1

def caller(target, value):
    alias = target
    assigned = lambda item: item + 2
    target(value)
    alias(value)
    assigned(value)
    value.real()

def use_target(value):
    return target(value)
"#;
    let report = report(source, Language::Python);
    let calls = &report.evidence.call_graph;
    assert!(calls.coverage.call_sites >= 5);
    assert!(calls.edges.iter().any(|edge| {
        edge.callee_name == "target"
            && edge.reason == kompass::evidence::CallResolutionReason::Parameter
            && edge.resolution == kompass::evidence::CallResolution::Unresolved
    }));
    assert!(calls.edges.iter().any(|edge| {
        edge.callee_name == "alias" && edge.reason == kompass::evidence::CallResolutionReason::Alias
    }));
    assert!(calls.edges.iter().any(|edge| {
        edge.callee_name == "real" && edge.reason == kompass::evidence::CallResolutionReason::Method
    }));
    assert!(calls.edges.iter().any(|edge| {
        edge.callee_name == "target"
            && edge.resolution == kompass::evidence::CallResolution::Resolved
    }));
}

#[test]
fn exact_duplicate_evidence_ignores_layout_but_keeps_identifiers_and_literals() {
    let rust = report(
        r#"
fn one(value: i32) -> i32 {
    let first = value + 1 + 2 + 3 + 4 + 5;
    let second = first * 2 + 3 + 4 + 5 + 6;
    let third = second + first + value + 7 + 8 + 9;
    third
}

fn two(value: i32) -> i32 {
    let first = value + 1 + 2 + 3 + 4 + 5; // comments are ignored
    let second = first * 2 + 3 + 4 + 5 + 6;
    let third = second + first + value + 7 + 8 + 9;
    third
}
"#,
        Language::Rust,
    );
    let group = rust.evidence.duplicates.groups.first().unwrap();
    assert_eq!(group.statement_count, 4);
    assert!(group.tokens >= 40);
    assert_eq!(group.occurrences.len(), 2);

    let python = report(
        r#"
def one(value):
    first = value + 1 + 2 + 3 + 4 + 5
    second = first * 2 + 3 + 4 + 5 + 6
    third = second + first + value + 7 + 8 + 9
    return third

def two(value):
    first = value + 1 + 2 + 3 + 4 + 5  # layout and comments do not matter
    second = first * 2 + 3 + 4 + 5 + 6
    third = second + first + value + 7 + 8 + 9
    return third
"#,
        Language::Python,
    );
    let group = python.evidence.duplicates.groups.first().unwrap();
    assert_eq!(group.statement_count, 4);
    assert!(group.tokens >= 40);
    assert_eq!(group.occurrences.len(), 2);
}

#[test]
fn changed_identifier_or_literal_does_not_count_as_exact_duplication() {
    let report = report(
        r#"
fn one(value: i32) -> i32 {
    let first = value + 1 + 2 + 3 + 4 + 5;
    let second = first * 2 + 3 + 4 + 5 + 6;
    let third = second + first + value + 7 + 8 + 9;
    third
}

fn two(value: i32) -> i32 {
    let first = value + 1 + 2 + 3 + 4 + 5;
    let second = first * 2 + 3 + 4 + 5 + 7;
    let third = second + first + value + 7 + 8 + 9;
    third
}
"#,
        Language::Rust,
    );
    assert!(report.evidence.duplicates.groups.is_empty());
}
