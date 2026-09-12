// The holdout bodies are frozen comparison inputs, so narrowly allow the
// style lints they intentionally exercise without changing their source.
#[allow(clippy::let_and_return)]
#[allow(clippy::manual_clamp)]
#[path = "fixtures/readability/holdout_after.rs"]
mod after;
#[allow(clippy::manual_clamp)]
#[path = "fixtures/readability/holdout_before.rs"]
mod before;
#[path = "support/readability.rs"]
mod support;

const BEFORE_SOURCE: &str = include_str!("fixtures/readability/holdout_before.rs");
const AFTER_SOURCE: &str = include_str!("fixtures/readability/holdout_after.rs");

#[test]
fn held_out_guard_clauses_preserve_trace_and_reduce_v3_burden() {
    for a in [false, true] {
        for b in [false, true] {
            for c in [false, true] {
                assert_eq!(
                    before::evaluate(a, b, c),
                    after::evaluate(a, b, c),
                    "trace changed for ({a}, {b}, {c})"
                );
            }
        }
    }

    let before_score = support::function_snapshot(BEFORE_SOURCE, "evaluate");
    let after_score = support::function_snapshot(AFTER_SOURCE, "evaluate");
    support::print_comparison("holdout-evaluate", before_score, after_score);
    assert!(after_score.v3_units < before_score.v3_units);
}

#[test]
fn held_out_match_binding_preserves_every_byte_value_and_v3_cost() {
    for value in u8::MIN..=u8::MAX {
        assert_eq!(
            before::classify(value),
            after::classify(value),
            "classification changed for {value}"
        );
    }

    let before_score = support::function_snapshot(BEFORE_SOURCE, "classify");
    let after_score = support::function_snapshot(AFTER_SOURCE, "classify");
    support::print_comparison("holdout-classify", before_score, after_score);
    assert_eq!(before_score.v3_units, after_score.v3_units);
}

#[test]
fn held_out_closure_and_named_helper_have_equal_aggregate_cost() {
    for value in i16::MIN..=i16::MAX {
        assert_eq!(
            before::transform(value),
            after::transform(value),
            "transform changed for {value}"
        );
    }

    let before_score = support::selected_snapshot(BEFORE_SOURCE, &["transform"]);
    let after_score = support::selected_snapshot(AFTER_SOURCE, &["transform"]);
    support::print_comparison("holdout-transform", before_score, after_score);
    assert_eq!(before_score.v3_units, after_score.v3_units);
}

#[test]
fn held_out_named_arithmetic_preserves_checked_range_and_v3_cost() {
    for a in -20_i16..=20 {
        for b in -20_i16..=20 {
            for c in -20_i16..=20 {
                assert_eq!(
                    before::calculate(a, b, c),
                    after::calculate(a, b, c),
                    "calculation changed for ({a}, {b}, {c})"
                );
            }
        }
    }

    let before_score = support::function_snapshot(BEFORE_SOURCE, "calculate");
    let after_score = support::function_snapshot(AFTER_SOURCE, "calculate");
    support::print_comparison("holdout-calculate", before_score, after_score);
    assert_eq!(before_score.v3_units, after_score.v3_units);
}
