#![allow(dead_code)]

use std::collections::BTreeMap;

use kompass::{score, tokens};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{FnArg, Item, ItemFn};

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub units: usize,
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
        score::measure(
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
    let mut collector = CallableCollector::new(&syntax);
    collector.visit_file(&syntax);

    snapshot_for_callable_metrics(collector.metrics, lexed.total_tokens())
}

/// Sum only the named top-level callables and any callables nested in their
/// bodies. This keeps a before/after row focused on the code being discussed,
/// while still charging extracted helpers and closures in aggregate scope.
pub fn selected_snapshot(source: &str, names: &[&str]) -> Snapshot {
    let syntax = syn::parse_file(source).expect("selected readability fixture should parse");
    let mut collector = CallableCollector::new(&syntax);
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

pub fn print_comparison(label: &str, before: Snapshot, after: Snapshot) {
    eprintln!(
        "readability/{label}: score {} -> {} (delta {:+}), tokens {} -> {}, callables {} -> {}",
        before.units,
        after.units,
        after.units as isize - before.units as isize,
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
    Snapshot {
        units: score::score(&metrics).units,
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
        aggregate.units = aggregate.units.saturating_add(snapshot.units);
        aggregate.callables = aggregate.callables.saturating_add(snapshot.callables);
    }
    aggregate
}

struct CallableCollector {
    metrics: Vec<kompass::model::Metrics>,
    closure_depths: BTreeMap<(tokens::TokenPosition, tokens::TokenPosition), usize>,
}

impl CallableCollector {
    fn new(syntax: &syn::File) -> Self {
        let closure_depths = score::closure_base_depths(syntax)
            .into_iter()
            .map(|closure| ((closure.start, closure.end), closure.depth))
            .collect();
        Self {
            metrics: Vec::new(),
            closure_depths,
        }
    }
}

impl<'ast> Visit<'ast> for CallableCollector {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.metrics.push(score::measure(
            &node.block,
            0,
            node.sig.inputs.len(),
            explicit_parameters(&node.sig.inputs),
        ));
        visit::visit_item_fn(self, node);
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        let span = node.span();
        let start = span.start();
        let end = span.end();
        let key = (
            tokens::TokenPosition {
                line: start.line,
                column: start.column,
            },
            tokens::TokenPosition {
                line: end.line,
                column: end.column,
            },
        );
        let base_depth = self.closure_depths.get(&key).copied().unwrap_or(0);
        self.metrics.push(score::measure_closure_at_depth(
            &node.body,
            0,
            node.inputs.len(),
            base_depth,
        ));
        visit::visit_expr_closure(self, node);
    }
}
