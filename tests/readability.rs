// These source shapes are intentional before/after fixtures; keep their
// nested and duplicated forms intact so the scorer sees the transformation.
#[allow(clippy::manual_range_contains)]
#[path = "fixtures/readability/after.rs"]
mod after;
#[allow(clippy::collapsible_if)]
#[allow(clippy::if_same_then_else)]
#[allow(clippy::needless_return)]
#[path = "fixtures/readability/before.rs"]
mod before;
#[path = "fixtures/readability/redistribution_after_left.rs"]
mod redistribution_after_left;
#[path = "fixtures/readability/redistribution_after_right.rs"]
mod redistribution_after_right;
#[path = "fixtures/readability/redistribution_before.rs"]
mod redistribution_before;
#[path = "support/readability.rs"]
mod support;

const BEFORE_SOURCE: &str = include_str!("fixtures/readability/before.rs");
const AFTER_SOURCE: &str = include_str!("fixtures/readability/after.rs");
const REDISTRIBUTION_BEFORE_SOURCE: &str =
    include_str!("fixtures/readability/redistribution_before.rs");
const REDISTRIBUTION_AFTER_LEFT_SOURCE: &str =
    include_str!("fixtures/readability/redistribution_after_left.rs");
const REDISTRIBUTION_AFTER_RIGHT_SOURCE: &str =
    include_str!("fixtures/readability/redistribution_after_right.rs");

#[test]
fn readability_transformations_preserve_behavior_on_edge_ranges() {
    let values = [i32::MIN, -101, -1, 0, 1, 9, 10, 100, 101, i32::MAX];
    for value in values {
        assert_eq!(
            before::guard_clauses(value),
            after::guard_clauses(value),
            "guard clauses changed behavior for {value}"
        );
        assert_eq!(
            before::named_intermediates(value),
            after::named_intermediates(value),
            "named intermediates changed behavior for {value}"
        );
        assert_eq!(
            before::useful_extraction(value),
            after::useful_extraction(value),
            "useful extraction changed behavior for {value}"
        );
        assert_eq!(
            before::pointless_wrapper(value),
            after::pointless_wrapper(value),
            "wrapper extraction changed behavior for {value}"
        );
        assert_eq!(
            before::deduplication(value),
            after::deduplication(value),
            "deduplication changed behavior for {value}"
        );
        assert_eq!(
            before::duplicate_left(value),
            after::duplicate_left(value),
            "left deduplication changed behavior for {value}"
        );
        assert_eq!(
            before::duplicate_right(value),
            after::duplicate_right(value),
            "right deduplication changed behavior for {value}"
        );
        assert_eq!(
            before::formatting(value),
            after::formatting(value),
            "formatting changed behavior for {value}"
        );
    }

    for values in [
        Vec::new(),
        vec![-10, 0, 10],
        vec![1, 2, 3, -4],
        vec![i32::MIN, i32::MAX],
    ] {
        assert_eq!(
            before::loop_alternative(&values),
            after::loop_alternative(&values),
            "loop and iterator alternatives diverged for {values:?}"
        );
    }

    for value in u8::MIN..=u8::MAX {
        assert_eq!(
            before::match_alternative(value),
            after::match_alternative(value),
            "match alternative changed behavior for {value}"
        );
    }

    for value in [-100_i64, -1, 0, 1, 100] {
        assert_eq!(
            before::branch_free(value),
            after::branch_free(value),
            "branch-free arithmetic changed behavior for {value}"
        );
    }

    for value in [i32::MIN, -1, 0, 1, i32::MAX] {
        assert_eq!(
            redistribution_before::redistributed_a(value),
            redistribution_after_left::redistributed_a(value)
        );
        assert_eq!(
            redistribution_before::redistributed_b(value),
            redistribution_after_right::redistributed_b(value)
        );
    }
}

#[test]
fn deduplicated_callers_preserve_behavior() {
    for value in [i32::MIN, -1, 0, 1, 100, i32::MAX] {
        assert_eq!(before::duplicate_left(value), after::duplicate_left(value));
        assert_eq!(
            before::duplicate_right(value),
            after::duplicate_right(value)
        );
    }
}

#[test]
fn subjective_readability_cases_report_reproducible_score_deltas() {
    // These rows are preferences to review with humans. The test records the
    // formula's response without asserting that every style preference is a
    // universal readability truth.
    for (label, name) in [
        ("guard-clauses", "guard_clauses"),
        ("named-intermediates", "named_intermediates"),
        ("deduplication", "deduplication"),
        ("loop-alternative", "loop_alternative"),
        ("match-alternative", "match_alternative"),
    ] {
        let before = support::function_snapshot(BEFORE_SOURCE, name);
        let after = support::function_snapshot(AFTER_SOURCE, name);
        support::print_comparison(label, before, after);
    }

    let useful_before = support::selected_snapshot(BEFORE_SOURCE, &["useful_extraction"]);
    let useful_after = support::selected_snapshot(AFTER_SOURCE, &["in_range", "useful_extraction"]);
    support::print_comparison("useful-extraction", useful_before, useful_after);
    assert!(
        useful_after.units < useful_before.units,
        "the extracted nested subtree should reduce aggregate burden"
    );

    let wrapper_before = support::selected_snapshot(BEFORE_SOURCE, &["pointless_wrapper"]);
    let wrapper_after =
        support::selected_snapshot(AFTER_SOURCE, &["apply_offset", "pointless_wrapper"]);
    support::print_comparison("pointless-wrapper", wrapper_before, wrapper_after);
    assert!(
        wrapper_after.units > wrapper_before.units,
        "a pointless forwarding helper should increase aggregate burden"
    );

    let duplicates_before =
        support::selected_snapshot(BEFORE_SOURCE, &["duplicate_left", "duplicate_right"]);
    let duplicates_after = support::selected_snapshot(
        AFTER_SOURCE,
        &["shared_positive", "duplicate_left", "duplicate_right"],
    );
    support::print_comparison("deduplicated-callers", duplicates_before, duplicates_after);
}

#[test]
fn structural_invariants_keep_score_stable() {
    let before_formatting = support::function_snapshot(BEFORE_SOURCE, "formatting");
    let after_formatting = support::function_snapshot(AFTER_SOURCE, "formatting");
    assert_eq!(
        before_formatting.units, after_formatting.units,
        "formatting-only change should preserve score"
    );

    let before_names = support::function_snapshot(BEFORE_SOURCE, "named_intermediates");
    let after_names = support::function_snapshot(AFTER_SOURCE, "named_intermediates");
    assert_eq!(
        before_names.units, after_names.units,
        "free bindings should remain outside the score"
    );

    let before_arithmetic = support::function_snapshot(BEFORE_SOURCE, "branch_free");
    let after_arithmetic = support::function_snapshot(AFTER_SOURCE, "branch_free");
    assert_eq!(
        before_arithmetic.units, after_arithmetic.units,
        "equivalent arithmetic should have the same operation count"
    );
    assert!(after_arithmetic.tokens > before_arithmetic.tokens);

    let before_files = support::aggregate_snapshot(REDISTRIBUTION_BEFORE_SOURCE);
    let after_left = support::aggregate_snapshot(REDISTRIBUTION_AFTER_LEFT_SOURCE);
    let after_right = support::aggregate_snapshot(REDISTRIBUTION_AFTER_RIGHT_SOURCE);
    assert_eq!(
        before_files.units,
        after_left.units + after_right.units,
        "moving callables between files must preserve additive burden"
    );
}
