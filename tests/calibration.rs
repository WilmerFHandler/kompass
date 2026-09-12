//! Structural regression and guardrail examples for structural-v2.
//!
//! These are deliberately original before/after examples rather than a
//! benchmark copied from another tool. They protect score direction,
//! invariances, aggregation, and opacity guardrails. The clarity examples are
//! subjective hypotheses and the cost cases are cost-sensitivity probes;
//! this corpus is not human calibration and does not establish readability or
//! developer-time predictions.

use kompass::score;
use syn::visit::{self, Visit};
use syn::{FnArg, ItemFn};

#[derive(Clone, Copy, Debug)]
enum Scope {
    Function,
    Aggregate,
}

#[derive(Clone, Copy, Debug)]
enum Expectation {
    Lower,
    Equal,
    Higher,
    NonDecreasing,
    Opaque,
}

#[derive(Clone, Copy, Debug)]
struct Pair {
    label: &'static str,
    before: &'static str,
    after: &'static str,
    scope: Scope,
    expectation: Expectation,
}

fn explicit_parameters(item: &ItemFn) -> usize {
    item.sig
        .inputs
        .iter()
        .filter(|input| !matches!(input, FnArg::Receiver(_)))
        .count()
}

fn function_score(source: &str) -> usize {
    let item: ItemFn = syn::parse_str(source).expect("corpus function must parse");
    score::score_v2(&score::measure_exclusive(
        &item.block,
        0,
        item.sig.inputs.len(),
        explicit_parameters(&item),
    ))
    .value
}

#[derive(Default)]
struct AggregateScorer {
    total: usize,
    macro_calls: usize,
}

impl AggregateScorer {
    fn add_function(&mut self, signature: &syn::Signature, block: &syn::Block) {
        let metrics = score::measure_exclusive(
            block,
            0,
            signature.inputs.len(),
            signature
                .inputs
                .iter()
                .filter(|input| !matches!(input, FnArg::Receiver(_)))
                .count(),
        );
        self.total = self.total.saturating_add(score::score_v2(&metrics).value);
    }
}

impl<'ast> Visit<'ast> for AggregateScorer {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.add_function(&node.sig, &node.block);
        visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.add_function(&node.sig, &node.block);
        visit::visit_impl_item_fn(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if let Some(block) = &node.default {
            self.add_function(&node.sig, block);
        }
        visit::visit_trait_item_fn(self, node);
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        let metrics = score::measure_closure(&node.body, 0, node.inputs.len());
        self.total = self.total.saturating_add(score::score_v2(&metrics).value);
        visit::visit_expr_closure(self, node);
    }

    fn visit_expr_macro(&mut self, node: &'ast syn::ExprMacro) {
        self.macro_calls += 1;
        visit::visit_expr_macro(self, node);
    }

    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        self.macro_calls += 1;
        visit::visit_stmt_macro(self, node);
    }
}

fn aggregate(source: &str) -> (usize, usize) {
    let syntax = syn::parse_file(source).expect("corpus file must parse");
    let mut scorer = AggregateScorer::default();
    scorer.visit_file(&syntax);
    (scorer.total, scorer.macro_calls)
}

fn measured(pair: &Pair, source: &str) -> (usize, usize) {
    match pair.scope {
        Scope::Function => (function_score(source), 0),
        Scope::Aggregate => aggregate(source),
    }
}

fn check_group(name: &str, pairs: &[Pair]) {
    for pair in pairs {
        let (before, before_macros) = measured(pair, pair.before);
        let (after, after_macros) = measured(pair, pair.after);
        match pair.expectation {
            Expectation::Lower => assert!(
                after < before,
                "{name}/{} expected a lower score: before={before}, after={after}",
                pair.label
            ),
            Expectation::Equal => assert_eq!(
                after, before,
                "{name}/{} expected equal scores: before={before}, after={after}",
                pair.label
            ),
            Expectation::Higher => assert!(
                after > before,
                "{name}/{} expected a higher score: before={before}, after={after}",
                pair.label
            ),
            Expectation::NonDecreasing => assert!(
                after >= before,
                "{name}/{} should not reduce aggregate burden: before={before}, after={after}",
                pair.label
            ),
            Expectation::Opaque => {
                assert_eq!(before_macros, 0, "{} unexpectedly has a macro", pair.label);
                assert!(after_macros > 0, "{} lost the opacity signal", pair.label);
                assert!(
                    after < before,
                    "{name}/{} should expose the macro opacity gap: before={before}, after={after}",
                    pair.label
                );
            }
        }
    }
}

// Subjective readability hypotheses: a human review should confirm these
// before the examples are treated as product evidence.
const CLARITY_HYPOTHESES: &[Pair] = &[
    Pair {
        label: "guard-clauses",
        before: "fn f(a: bool, b: bool, c: bool) { if a { if b { if c { work(); } } } }",
        after: "fn f(a: bool, b: bool, c: bool) { if !a { return; } if !b { return; } if !c { return; } work(); }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "loop-continue",
        before: "fn f(items: &[bool]) { for item in items { if *item { if ready() { work(); } } } }",
        after: "fn f(items: &[bool]) { for item in items { if !*item { continue; } if !ready() { continue; } work(); } }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "boolean-flatness",
        before: "fn f(a: bool, b: bool, c: bool) { if a { if b { if c { work(); } } } }",
        after: "fn f(a: bool, b: bool, c: bool) { if a && b && c { work(); } }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "match-early-return",
        before: "fn f(value: Option<i32>) { if value.is_some() { if value.unwrap() > 0 { if value.unwrap() < 10 { work(); } } } }",
        after: "fn f(value: Option<i32>) { let Some(number) = value else { return; }; if !(1..10).contains(&number) { return; } work(); }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "if-chain-to-match",
        before: "fn f(value: u8) { if value == 0 { zero(); } else if value == 1 { one(); } else if value == 2 { two(); } else { other(); } }",
        after: "fn f(value: u8) { match value { 0 => zero(), 1 => one(), 2 => two(), _ => other(), } }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "let-else-flattening",
        before: "fn f(value: Option<i32>) { if value.is_some() { let number = value.unwrap(); if number > 0 { if number < 10 { work(); } } } }",
        after: "fn f(value: Option<i32>) { let Some(number) = value else { return; }; if number <= 0 || number >= 10 { return; } work(); }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "nested-loops",
        before: "fn f(rows: &[Vec<i32>]) { for row in rows { for value in row { if *value > 0 { consume(*value); } } } }",
        after: "fn f(rows: &[Vec<i32>]) { for row in rows { for value in row.iter().filter(|value| **value > 0) { consume(*value); } } }",
        scope: Scope::Aggregate,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "match-guard",
        before: "fn f(value: Option<Result<i32, E>>) { match value { Some(result) => { if result.is_ok() { if *result.as_ref().unwrap() > 0 { work(); } } }, None => {} } }",
        after: "fn f(value: Option<Result<i32, E>>) { match value { Some(Ok(number)) if number > 0 => work(), _ => {} } }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "nested-option",
        before: "fn f(value: Option<Option<bool>>) { if let Some(inner) = value { if let Some(flag) = inner { if flag { work(); } } } }",
        after: "fn f(value: Option<Option<bool>>) { if value == Some(Some(true)) { work(); } }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "nested-state",
        before: "fn f(state: State) { if state.is_ready() { if state.has_input() { if state.is_valid() { execute(); } } } }",
        after: "fn f(state: State) { if !(state.is_ready() && state.has_input() && state.is_valid()) { return; } execute(); }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "iterator-filter",
        before: "fn f(items: &[Item]) { for item in items { if item.enabled() { if item.valid() { consume(item); } } } }",
        after: "fn f(items: &[Item]) { items.iter().filter(|item| item.enabled() && item.valid()).for_each(consume); }",
        scope: Scope::Aggregate,
        expectation: Expectation::Lower,
    },
    Pair {
        label: "guarded-match",
        before: "fn f(value: Result<i32, E>) { if value.is_ok() { match value.unwrap() { 0 => zero(), number => { if number > 0 { positive(number); } } } } }",
        after: "fn f(value: Result<i32, E>) { match value { Ok(0) => zero(), Ok(number) if number > 0 => positive(number), _ => {} } }",
        scope: Scope::Function,
        expectation: Expectation::Lower,
    },
];

// Structural invariants: formatting, naming, and ordering should not change
// the measured cost when the source shape and behavior remain equivalent.
const STRUCTURAL_INVARIANTS: &[Pair] = &[
    Pair {
        label: "comments",
        before: "fn f(value: bool) { if value { work(); } }",
        after: "fn f(value: bool) { /* explanation */ if value { // branch\n work(); } }",
        scope: Scope::Function,
        expectation: Expectation::Equal,
    },
    Pair {
        label: "formatting",
        before: "fn f(value: bool) { if value { work(); } }",
        after: "fn f(value: bool) {\n    if value {\n        work();\n    }\n}",
        scope: Scope::Function,
        expectation: Expectation::Equal,
    },
    Pair {
        label: "renaming",
        before: "fn f(value: bool) { if value { work(value); } }",
        after: "fn f(input: bool) { if input { work(input); } }",
        scope: Scope::Function,
        expectation: Expectation::Equal,
    },
    Pair {
        label: "attributes",
        before: "fn f(value: bool) { if value { work(); } }",
        after: "#[inline]\n#[allow(clippy::needless_return)]\nfn f(value: bool) { if value { work(); } }",
        scope: Scope::Function,
        expectation: Expectation::Equal,
    },
    Pair {
        label: "independent-if-order",
        before: "fn f(a: bool, b: bool) { if a {} if b {} }",
        after: "fn f(a: bool, b: bool) { if b {} if a {} }",
        scope: Scope::Function,
        expectation: Expectation::Equal,
    },
    Pair {
        label: "match-arm-order",
        before: "fn f(value: u8) { match value { 0 => zero(), 1 => one(), _ => other(), } }",
        after: "fn f(value: u8) { match value { 1 => one(), 0 => zero(), _ => other(), } }",
        scope: Scope::Function,
        expectation: Expectation::Equal,
    },
    Pair {
        label: "parentheses",
        before: "fn f(a: bool, b: bool) { if a && b { work(); } }",
        after: "fn f(a: bool, b: bool) { if (a && b) { work(); } }",
        scope: Scope::Function,
        expectation: Expectation::Equal,
    },
    Pair {
        label: "closure-renaming-aggregate",
        before: "fn f(items: &[i32]) { items.iter().map(|value| value + 1).for_each(use_value); }",
        after: "fn f(items: &[i32]) { items.iter().map(|item| item + 1).for_each(use_value); }",
        scope: Scope::Aggregate,
        expectation: Expectation::Equal,
    },
    Pair {
        // Useful extraction of one nested subtree; aggregate scope includes
        // both callable boundaries and the caller's new call site.
        label: "nested-subtree-extraction",
        before: "fn run(outer: bool, value: bool) { if outer { if value { if value { work(); } } } }",
        after: "fn inner(value: bool) { if value { if value { work(); } } }\nfn run(outer: bool, value: bool) { if outer { inner(value); } }",
        scope: Scope::Aggregate,
        expectation: Expectation::Lower,
    },
];

// Guardrails and aggregation probes: these exercise movement, fragmentation,
// callable boundaries, and macro opacity without assigning a macro weight.
const GUARDRAIL_PROBES: &[Pair] = &[
    Pair {
        label: "comment-padding",
        before: "fn f(value: bool) { if value { work(); } }",
        after: "fn f(value: bool) { /* many words that add lines but no behavior */ if value { work(); } }",
        scope: Scope::Function,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        label: "blank-line-padding",
        before: "fn f(value: bool) { if value { work(); } }",
        after: "fn f(value: bool) {\n\n\n    if value {\n        work();\n    }\n}",
        scope: Scope::Function,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        label: "short-identifiers",
        before: "fn process_request(request_is_ready: bool) { if request_is_ready { handle_request(); } }",
        after: "fn p(r: bool) { if r { h(); } }",
        scope: Scope::Function,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        label: "attribute-padding",
        before: "fn f(value: bool) { if value { work(); } }",
        after: "#[inline]\n#[cold]\n#[must_use]\nfn f(value: bool) { if value { work(); } }",
        scope: Scope::Function,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        // Harmful fragmentation inserts several tiny forwarding boundaries.
        label: "harmful-fragmentation",
        before: "fn run(value: bool) { if value { if value { work(); } } }",
        after: "fn first(value: bool) { if value { work(); } }\nfn second(value: bool) { first(value); }\nfn third(value: bool) { second(value); }\nfn run(value: bool) { third(value); }",
        scope: Scope::Aggregate,
        expectation: Expectation::Higher,
    },
    Pair {
        // Whole-function movement isolates one existing body in a helper; it
        // is kept separate from nested-subtree extraction above.
        label: "function-movement",
        before: "fn run(value: bool) { if value { if value { if value { work(); } } } }",
        after: "fn check(value: bool) { if value { if value { if value { work(); } } } }\nfn run(value: bool) { check(value); }",
        scope: Scope::Aggregate,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        label: "move-to-closure",
        before: "fn run(value: bool) { if value { if value { work(); } } }",
        after: "fn run(value: bool) { let work_if_ready = || if value { if value { work(); } }; work_if_ready(); }",
        scope: Scope::Aggregate,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        label: "closure-to-helper",
        before: "fn run(value: bool) { let check = || if value { work(); }; check(); }",
        after: "fn helper(value: bool) { if value { work(); } }\nfn run(value: bool) { helper(value); }",
        scope: Scope::Aggregate,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        label: "function-order",
        before: "fn first(value: bool) { if value { work(); } }\nfn second(value: bool) { if value { other(); } }",
        after: "fn second(value: bool) { if value { other(); } }\nfn first(value: bool) { if value { work(); } }",
        scope: Scope::Aggregate,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        label: "dead-branch",
        before: "fn run(value: bool) { if value { work(); } }",
        after: "fn run(value: bool) { if false { unreachable_work(); } if value { work(); } }",
        scope: Scope::Function,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        label: "return-padding",
        before: "fn run(value: bool) { if value { work(); } }",
        after: "fn run(value: bool) { if value { work(); return; } return; }",
        scope: Scope::Function,
        expectation: Expectation::NonDecreasing,
    },
    Pair {
        label: "macro-opacity",
        before: "fn run(value: bool) { if value { if value { if value { work(); } } } }",
        after: "macro_rules! hidden { ($value:expr) => { if $value { if $value { if $value { work(); } } } }; }\nfn run(value: bool) { hidden!(value); }",
        scope: Scope::Aggregate,
        expectation: Expectation::Opaque,
    },
];

// Cost-sensitivity probes: these intentionally add arms, calls, statements,
// parameters, or callable boundaries. A higher score tests formula response,
// not a claim that the resulting code is less readable.
const COST_SENSITIVITY_PROBES: &[Pair] = &[
    Pair {
        label: "exhaustive-match",
        before: "fn classify(value: u8) { if value == 0 { zero(); } else { other(); } }",
        after: "fn classify(value: u8) { match value { 0 => zero(), 1 => one(), 2 => two(), 3 => three(), 4 => four(), _ => other(), } }",
        scope: Scope::Function,
        expectation: Expectation::Higher,
    },
    Pair {
        label: "explicit-api",
        before: "fn configure(value: Config) { apply(value); }",
        after: "fn configure(a: bool, b: bool, c: bool, d: bool, e: bool) { apply(a, b, c, d, e); }",
        scope: Scope::Function,
        expectation: Expectation::Higher,
    },
    Pair {
        label: "iterator-call-chain",
        before: "fn collect(items: &[Item]) { for item in items { use_item(item); } }",
        after: "fn collect(items: &[Item]) { items.iter().filter(valid).map(convert).inspect(record).flat_map(expand).enumerate().skip(1).take(10).collect::<Vec<_>>(); }",
        scope: Scope::Function,
        expectation: Expectation::Higher,
    },
    Pair {
        label: "straight-line-work",
        before: "fn prepare(value: Value) { if value.needs_work() { work(value); } }",
        after: "fn prepare(value: Value) { let one = step_one(value); let two = step_two(one); let three = step_three(two); let four = step_four(three); let five = step_five(four); use_value(five); }",
        scope: Scope::Function,
        expectation: Expectation::Higher,
    },
    Pair {
        label: "many-valid-arms",
        before: "fn route(value: Route) { if value.is_valid() { route_valid(value); } else { route_invalid(value); } }",
        after: "fn route(value: Route) { match value { Route::A => route_a(), Route::B => route_b(), Route::C => route_c(), Route::D => route_d(), Route::E => route_e(), Route::F => route_f(), Route::G => route_g(), } }",
        scope: Scope::Function,
        expectation: Expectation::Higher,
    },
    Pair {
        label: "builder-chain",
        before: "fn build(input: Input) { if input.is_valid() { create(input); } }",
        after: "fn build(input: Input) { Builder::new().with_name(input.name()).with_mode(input.mode()).with_limit(input.limit()).with_cache(input.cache()).finish(); }",
        scope: Scope::Function,
        expectation: Expectation::Higher,
    },
    Pair {
        label: "explicit-validation",
        before: "fn validate(value: Value) { if value.is_valid() { accept(value); } }",
        after: "fn validate(value: Value) { if value.is_missing() { return; } if value.is_expired() { return; } if value.is_malformed() { return; } if value.is_unauthorized() { return; } accept(value); }",
        scope: Scope::Function,
        expectation: Expectation::Higher,
    },
    Pair {
        label: "split-file-burden",
        before: "fn run(value: bool) { if value { work(); } }",
        after: "fn check(value: bool) { if value { work(); } }\nfn run(value: bool) { check(value); }\nfn audit(value: bool) { check(value); }\nfn report(value: bool) { check(value); }",
        scope: Scope::Aggregate,
        expectation: Expectation::Higher,
    },
];

#[test]
fn structural_corpus_has_the_required_four_groups() {
    assert!(CLARITY_HYPOTHESES.len() >= 12);
    assert!(STRUCTURAL_INVARIANTS.len() >= 8);
    assert!(GUARDRAIL_PROBES.len() >= 12);
    assert!(COST_SENSITIVITY_PROBES.len() >= 8);
    assert!(
        CLARITY_HYPOTHESES.len()
            + STRUCTURAL_INVARIANTS.len()
            + GUARDRAIL_PROBES.len()
            + COST_SENSITIVITY_PROBES.len()
            >= 40
    );
}

#[test]
fn clarity_hypotheses_have_lower_structural_cost() {
    check_group("clarity hypothesis", CLARITY_HYPOTHESES);
}

#[test]
fn structural_invariants_preserve_complexity() {
    check_group("structural invariant", STRUCTURAL_INVARIANTS);
}

#[test]
fn guardrail_probes_do_not_lower_burden_without_opacity() {
    check_group("guardrail", GUARDRAIL_PROBES);
}

#[test]
fn cost_sensitivity_probes_can_cost_more() {
    check_group("cost sensitivity", COST_SENSITIVITY_PROBES);
}
