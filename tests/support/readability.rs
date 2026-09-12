#![allow(dead_code)]

use kompass::{score, tokens};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{FnArg, Item, ItemFn};

/// The small, copyable view used by the readability tests. The full syntax
/// tree remains in the fixture module; this view makes coefficient diagnostics
/// explicit without duplicating the production formula in assertions.
#[derive(Clone, Copy, Debug, Default)]
pub struct MetricCounts {
    pub control_decisions: usize,
    pub nesting_penalty: usize,
    pub boolean_operators: usize,
    pub statements: usize,
    pub call_sites: usize,
    pub explicit_parameters: usize,
    pub match_arms: usize,
    pub expression_operations: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub metrics: MetricCounts,
    pub v2_units: usize,
    pub v3_units: usize,
    pub tokens: usize,
    pub callables: usize,
}

pub fn function_snapshot(source: &str, name: &str) -> Snapshot {
    let syntax = syn::parse_file(source).unwrap_or_else(|error| {
        panic!("fixture {name} should parse: {error}");
    });
    let item = syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Fn(item) if item.sig.ident == name => Some(item),
            _ => None,
        })
        .unwrap_or_else(|| panic!("fixture does not contain top-level fn {name}"));
    snapshot_for_metrics(
        score::measure_exclusive(
            &item.block,
            0,
            item.sig.inputs.len(),
            explicit_parameters(&item.sig.inputs),
        ),
        tokens_in_span(source, item.span()),
        1,
    )
}

/// Sum exclusive callable scores for a whole fixture. This is the required
/// scope for transformations that move work into a helper or a closure: the
/// new callable boundary and its body both remain visible in the aggregate.
pub fn aggregate_snapshot(source: &str) -> Snapshot {
    let syntax = syn::parse_file(source).expect("aggregate readability fixture should parse");
    let lexed = tokens::lex(source).expect("aggregate readability fixture should lex");
    let mut collector = CallableCollector::default();
    collector.visit_file(&syntax);

    snapshot_for_callable_metrics(collector.metrics, lexed.total_tokens())
}

/// Sum only the named top-level callables and any callables nested in their
/// bodies. This keeps a before/after row focused on the code being discussed,
/// while still charging extracted helpers and closures in aggregate scope.
pub fn selected_snapshot(source: &str, names: &[&str]) -> Snapshot {
    let syntax = syn::parse_file(source).expect("selected readability fixture should parse");
    let mut collector = CallableCollector::default();
    let mut selected_tokens: usize = 0;
    for item in &syntax.items {
        let Item::Fn(item) = item else {
            continue;
        };
        if names.iter().any(|name| item.sig.ident == *name) {
            selected_tokens = selected_tokens.saturating_add(tokens_in_span(source, item.span()));
            collector.visit_item_fn(item);
        }
    }

    snapshot_for_callable_metrics(collector.metrics, selected_tokens)
}

/// The v3 candidate with a variable expression-operation coefficient. All
/// other structural-v2 coefficients stay fixed, and the existing aggregate
/// v2 score supplies one boundary per callable without reimplementing that
/// boundary term here.
pub fn candidate_units(snapshot: Snapshot, expression_operation_weight: usize) -> usize {
    snapshot
        .v2_units
        .saturating_sub(snapshot.metrics.statements)
        .saturating_add(
            snapshot
                .metrics
                .expression_operations
                .saturating_mul(expression_operation_weight),
        )
}

pub fn print_comparison(label: &str, before: Snapshot, after: Snapshot) {
    eprintln!(
        "readability/{label}: v2 {} -> {} (delta {:+}), v3 {} -> {} (delta {:+}), tokens {} -> {}, callables {} -> {}",
        before.v2_units,
        after.v2_units,
        after.v2_units as isize - before.v2_units as isize,
        before.v3_units,
        after.v3_units,
        after.v3_units as isize - before.v3_units as isize,
        before.tokens,
        after.tokens,
        before.callables,
        after.callables,
    );
}

fn explicit_parameters(inputs: &syn::punctuated::Punctuated<FnArg, syn::token::Comma>) -> usize {
    inputs
        .iter()
        .filter(|input| matches!(input, FnArg::Typed(_)))
        .count()
}

fn tokens_in_span(source: &str, span: proc_macro2::Span) -> usize {
    let lexed = tokens::lex(source).expect("readability fixture should lex");
    let start = span.start();
    let end = span.end();
    lexed.tokens_in(
        tokens::TokenPosition {
            line: start.line,
            column: start.column,
        },
        tokens::TokenPosition {
            line: end.line,
            column: end.column,
        },
    )
}

fn snapshot_for_metrics(
    metrics: kompass::model::Metrics,
    tokens: usize,
    callables: usize,
) -> Snapshot {
    let v2_units = score::score_v2(&metrics).units;
    let v3_units = score::score_v3(&metrics).units;
    Snapshot {
        metrics: MetricCounts {
            control_decisions: metrics.control_decisions,
            nesting_penalty: metrics.nesting_penalty,
            boolean_operators: metrics.boolean_operators,
            statements: metrics.statements,
            call_sites: metrics.call_sites,
            explicit_parameters: metrics.explicit_parameters,
            match_arms: metrics.match_arms,
            expression_operations: metrics.expression_operations,
        },
        v2_units,
        v3_units,
        tokens,
        callables,
    }
}

fn snapshot_for_callable_metrics(metrics: Vec<kompass::model::Metrics>, tokens: usize) -> Snapshot {
    let mut aggregate = Snapshot {
        tokens,
        ..Snapshot::default()
    };
    for metrics in metrics {
        let snapshot = snapshot_for_metrics(metrics, 0, 1);
        aggregate.metrics = add_metrics(aggregate.metrics, snapshot.metrics);
        aggregate.v2_units = aggregate.v2_units.saturating_add(snapshot.v2_units);
        aggregate.v3_units = aggregate.v3_units.saturating_add(snapshot.v3_units);
        aggregate.callables = aggregate.callables.saturating_add(snapshot.callables);
    }
    aggregate
}

fn add_metrics(left: MetricCounts, right: MetricCounts) -> MetricCounts {
    MetricCounts {
        control_decisions: left
            .control_decisions
            .saturating_add(right.control_decisions),
        nesting_penalty: left.nesting_penalty.saturating_add(right.nesting_penalty),
        boolean_operators: left
            .boolean_operators
            .saturating_add(right.boolean_operators),
        statements: left.statements.saturating_add(right.statements),
        call_sites: left.call_sites.saturating_add(right.call_sites),
        explicit_parameters: left
            .explicit_parameters
            .saturating_add(right.explicit_parameters),
        match_arms: left.match_arms.saturating_add(right.match_arms),
        expression_operations: left
            .expression_operations
            .saturating_add(right.expression_operations),
    }
}

#[derive(Default)]
struct CallableCollector {
    metrics: Vec<kompass::model::Metrics>,
}

impl<'ast> Visit<'ast> for CallableCollector {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.metrics.push(score::measure_exclusive(
            &node.block,
            0,
            node.sig.inputs.len(),
            explicit_parameters(&node.sig.inputs),
        ));
        visit::visit_item_fn(self, node);
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        self.metrics
            .push(score::measure_closure(&node.body, 0, node.inputs.len()));
        visit::visit_expr_closure(self, node);
    }
}
