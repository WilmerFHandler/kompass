use syn::visit::{self, Visit};
use syn::{
    Arm, BinOp, Expr, ExprAssign, ExprBinary, ExprCast, ExprIf, ExprIndex, ExprMatch,
    ExprReference, ExprUnary, ExprUnsafe, Stmt, UnOp,
};

use crate::model::{Metrics, Score, ScoringModel};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallableOwnership {
    /// Retain structural-v1's original inclusive closure traversal.
    #[default]
    Inclusive,
    /// Attribute every callable body to that callable exactly once.
    Exclusive,
}

/// Measure the structural signals that are available from a Rust syntax
/// tree without compiling or expanding the crate.
pub fn measure(block: &syn::Block, code_lines: usize, parameters: usize) -> Metrics {
    measure_with_ownership(
        block,
        code_lines,
        parameters,
        parameters,
        CallableOwnership::Inclusive,
    )
}

/// Measure a function body with exclusive callable ownership. Nested named
/// functions and closures are left to their own reports, so their bodies do
/// not change the enclosing function's metrics.
pub fn measure_exclusive(
    block: &syn::Block,
    code_lines: usize,
    parameters: usize,
    explicit_parameters: usize,
) -> Metrics {
    measure_with_ownership(
        block,
        code_lines,
        parameters,
        explicit_parameters,
        CallableOwnership::Exclusive,
    )
}

/// Measure a closure body as its own callable. A closure body can be either a
/// block or a single expression; the latter is one executable statement for
/// parity with an equivalent function body.
pub fn measure_closure(body: &Expr, code_lines: usize, explicit_parameters: usize) -> Metrics {
    let mut visitor = MetricsVisitor {
        ownership: CallableOwnership::Exclusive,
        ..MetricsVisitor::default()
    };
    visitor.metrics.code_lines = code_lines;
    visitor.metrics.parameters = explicit_parameters;
    visitor.metrics.explicit_parameters = explicit_parameters;
    if !matches!(body, Expr::Block(_)) {
        visitor.metrics.statements = 1;
    }
    visitor.visit_expr(body);
    visitor.metrics
}

fn measure_with_ownership(
    block: &syn::Block,
    code_lines: usize,
    parameters: usize,
    explicit_parameters: usize,
    ownership: CallableOwnership,
) -> Metrics {
    let mut visitor = MetricsVisitor {
        ownership,
        ..MetricsVisitor::default()
    };
    visitor.metrics.code_lines = code_lines;
    visitor.metrics.parameters = parameters;
    visitor.metrics.explicit_parameters = explicit_parameters;
    visitor.visit_block(block);
    visitor.metrics
}

/// Model 1 is intentionally additive and fixed. Every component is an
/// integer that is emitted with each function, so a score can be explained
/// without reverse engineering a hidden normalisation step. Tokens are
/// reported separately: their count is useful context, but does not affect
/// this model's score.
pub fn score(metrics: &Metrics) -> Score {
    let statement_penalty = metrics.statements / 10;
    let value = metrics
        .decisions
        .saturating_add(metrics.nesting_penalty)
        .saturating_add(statement_penalty);

    Score {
        value,
        units: value,
        display: value.to_string(),
        decisions: metrics.decisions,
        nesting_penalty: metrics.nesting_penalty,
        statement_penalty,
        ..Score::default()
    }
}

/// Structural-v2 is deliberately linear and transparent. The score is kept
/// as integer tenths so JSON and downstream tooling never lose precision;
/// text output can render those units as a one-decimal value.
pub fn score_v2(metrics: &Metrics) -> Score {
    score_tenth_model(metrics, metrics.statements, 0)
}

/// Structural-v3 keeps every structural-v2 charge except the statement
/// charge. Each counted expression operation contributes one integer tenth.
pub fn score_v3(metrics: &Metrics) -> Score {
    score_tenth_model(metrics, 0, metrics.expression_operations)
}

/// Score the shared integer-tenth components used by structural-v2 and
/// structural-v3. The model-specific charge is supplied explicitly so v3 is
/// calculated directly rather than subtracting a saturated v2 score.
fn score_tenth_model(
    metrics: &Metrics,
    statement_units: usize,
    expression_operation_units: usize,
) -> Score {
    let boundary: usize = 10;
    let control_decision_units = metrics.control_decisions.saturating_mul(10);
    let nesting_units = metrics.nesting_penalty.saturating_mul(10);
    let boolean_operator_units = metrics.boolean_operators.saturating_mul(5);
    let call_site_units = metrics.call_sites.saturating_mul(2);
    let parameter_units = metrics.explicit_parameters.saturating_mul(2);
    let match_arm_units = metrics.match_arms.saturating_mul(2);
    let value = boundary
        .saturating_add(control_decision_units)
        .saturating_add(nesting_units)
        .saturating_add(boolean_operator_units)
        .saturating_add(statement_units)
        .saturating_add(expression_operation_units)
        .saturating_add(call_site_units)
        .saturating_add(parameter_units)
        .saturating_add(match_arm_units);

    Score {
        value,
        units: value,
        display: format!("{:.1}", value as f64 / 10.0),
        boundary,
        decisions: metrics.control_decisions,
        control_decisions: metrics.control_decisions,
        nesting_penalty: metrics.nesting_penalty,
        statement_penalty: statement_units,
        boolean_operator_units,
        call_site_units,
        parameter_units,
        match_arm_units,
        expression_operation_units,
    }
}

pub fn score_for_model(metrics: &Metrics, model: ScoringModel) -> Score {
    match model {
        ScoringModel::StructuralV1 => score(metrics),
        ScoringModel::StructuralV2 => score_v2(metrics),
        ScoringModel::StructuralV3 => score_v3(metrics),
    }
}

#[derive(Default)]
struct MetricsVisitor {
    metrics: Metrics,
    depth: usize,
    ownership: CallableOwnership,
}

impl MetricsVisitor {
    fn branch(&mut self) {
        self.metrics.branches += 1;
        self.metrics.decisions += 1;
        self.metrics.control_decisions += 1;
        self.metrics.nesting_penalty += self.depth;
        self.metrics.max_depth = self.metrics.max_depth.max(self.depth + 1);
    }

    fn with_depth(&mut self, visit: impl FnOnce(&mut Self)) {
        self.depth += 1;
        visit(self);
        self.depth -= 1;
    }
}

impl<'ast> Visit<'ast> for MetricsVisitor {
    fn visit_expr_if(&mut self, node: &'ast ExprIf) {
        self.branch();
        self.visit_expr(&node.cond);
        self.with_depth(|visitor| visitor.visit_block(&node.then_branch));

        if let Some((_, else_branch)) = &node.else_branch {
            // An `else if` chain is a sequence of alternatives, rather than
            // the same kind of nested block as `if { if ... }`.
            if let Expr::If(else_if) = else_branch.as_ref() {
                self.visit_expr_if(else_if);
            } else {
                self.with_depth(|visitor| visitor.visit_expr(else_branch));
            }
        }
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        self.branch();
        self.metrics.match_arms += node.arms.len();
        self.visit_expr(&node.expr);
        self.with_depth(|visitor| {
            for arm in &node.arms {
                visitor.visit_arm(arm);
            }
        });
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.branch();
        self.metrics.loops += 1;
        self.with_depth(|visitor| visitor.visit_block(&node.body));
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.branch();
        self.metrics.loops += 1;
        self.visit_pat(&node.pat);
        self.visit_expr(&node.expr);
        self.with_depth(|visitor| visitor.visit_block(&node.body));
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.branch();
        self.metrics.loops += 1;
        self.visit_expr(&node.cond);
        self.with_depth(|visitor| visitor.visit_block(&node.body));
    }

    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        let short_circuit = matches!(node.op, BinOp::And(_) | BinOp::Or(_));
        if short_circuit {
            self.metrics.boolean_operators += 1;
            self.metrics.decisions += 1;
        } else {
            self.metrics.expression_operations += 1;
        }
        if matches!(
            node.op,
            BinOp::AddAssign(_)
                | BinOp::SubAssign(_)
                | BinOp::MulAssign(_)
                | BinOp::DivAssign(_)
                | BinOp::RemAssign(_)
                | BinOp::BitXorAssign(_)
                | BinOp::BitAndAssign(_)
                | BinOp::BitOrAssign(_)
                | BinOp::ShlAssign(_)
                | BinOp::ShrAssign(_)
        ) {
            self.metrics.mutations += 1;
        }
        visit::visit_expr_binary(self, node);
    }

    fn visit_expr_unary(&mut self, node: &'ast ExprUnary) {
        if matches!(node.op, UnOp::Neg(_) | UnOp::Not(_) | UnOp::Deref(_)) {
            self.metrics.expression_operations += 1;
        }
        visit::visit_expr_unary(self, node);
    }

    fn visit_expr_index(&mut self, node: &'ast ExprIndex) {
        self.metrics.expression_operations += 1;
        visit::visit_expr_index(self, node);
    }

    fn visit_expr_cast(&mut self, node: &'ast ExprCast) {
        self.metrics.expression_operations += 1;
        visit::visit_expr_cast(self, node);
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        self.metrics.closures += 1;
        if self.ownership == CallableOwnership::Inclusive {
            for input in &node.inputs {
                self.visit_pat(input);
            }
            // This is retained solely for the v1 compatibility path. The
            // v2 path treats the closure body as an independent callable.
            self.visit_expr(&node.body);
        }
    }

    fn visit_arm(&mut self, node: &'ast Arm) {
        if node.guard.is_some() {
            // A guard adds a decision at the match-arm depth. Boolean
            // operators inside it remain decisions without a nesting penalty.
            self.metrics.decisions += 1;
            self.metrics.control_decisions += 1;
            self.metrics.nesting_penalty += self.depth;
        }
        visit::visit_arm(self, node);
    }

    fn visit_expr_return(&mut self, node: &'ast syn::ExprReturn) {
        self.metrics.returns += 1;
        visit::visit_expr_return(self, node);
    }

    fn visit_expr_assign(&mut self, node: &'ast ExprAssign) {
        self.metrics.mutations += 1;
        self.metrics.expression_operations += 1;
        visit::visit_expr_assign(self, node);
    }

    fn visit_expr_reference(&mut self, node: &'ast ExprReference) {
        if node.mutability.is_some() {
            self.metrics.mutations += 1;
        }
        visit::visit_expr_reference(self, node);
    }

    fn visit_expr_unsafe(&mut self, node: &'ast ExprUnsafe) {
        self.metrics.unsafe_blocks += 1;
        visit::visit_expr_unsafe(self, node);
    }

    fn visit_expr_macro(&mut self, node: &'ast syn::ExprMacro) {
        self.metrics.macro_calls += 1;
        visit::visit_expr_macro(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        self.metrics.call_sites += 1;
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.metrics.call_sites += 1;
        visit::visit_expr_method_call(self, node);
    }

    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        self.metrics.macro_calls += 1;
        visit::visit_stmt_macro(self, node);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let Some(init) = &node.init
            && let Some((_, diverge)) = &init.diverge
        {
            self.branch();
            self.visit_pat(&node.pat);
            self.visit_expr(&init.expr);
            self.with_depth(|visitor| {
                if let Expr::Block(block) = diverge.as_ref() {
                    visitor.visit_block(&block.block);
                } else {
                    visitor.visit_expr(diverge);
                }
            });
            return;
        }
        visit::visit_local(self, node);
    }

    fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
        if node.mutability.is_some() {
            self.metrics.mutations += 1;
        }
        visit::visit_pat_ident(self, node);
    }

    fn visit_stmt(&mut self, node: &'ast Stmt) {
        // A nested item is analyzed independently by the function collector;
        // charging its statements to the containing function would double
        // count complexity and make splitting functions look worse.
        if matches!(node, Stmt::Item(_)) {
            return;
        }
        if matches!(node, Stmt::Local(_) | Stmt::Expr(_, _) | Stmt::Macro(_)) {
            self.metrics.statements += 1;
        }
        visit::visit_stmt(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(source: &str) -> Metrics {
        let item: syn::ItemFn = syn::parse_str(source).unwrap();
        measure(&item.block, 1, item.sig.inputs.len())
    }

    #[test]
    fn nesting_separates_flat_and_nested_conditionals() {
        let flat = metrics("fn f(a: bool, b: bool, c: bool) { if a {} if b {} if c {} }");
        let nested = metrics("fn f(a: bool, b: bool, c: bool) { if a { if b { if c {} } } }");

        assert_eq!(flat.branches, nested.branches);
        assert!(nested.max_depth > flat.max_depth);
        assert!(nested.nesting_penalty > flat.nesting_penalty);
        assert!(score(&nested).value > score(&flat).value);
    }

    #[test]
    fn match_arms_and_short_circuiting_are_visible() {
        let measured = metrics(
            "fn f(value: Option<bool>) { match value { Some(a) if a && true => {}, Some(_) => {}, None => {} } }",
        );

        assert_eq!(measured.match_arms, 3);
        assert_eq!(measured.boolean_operators, 1);
        assert_eq!(measured.branches, 1);
        assert_eq!(measured.decisions, 3);
        assert_eq!(measured.nesting_penalty, 1);
    }

    #[test]
    fn let_else_counts_as_a_branch_and_nests_its_diverging_block() {
        let measured = metrics(
            "fn f(value: Option<i32>) { let Some(value) = value else { if true { return; } return; }; if value > 0 {} }",
        );

        assert_eq!(measured.branches, 3);
        assert_eq!(measured.decisions, 3);
        assert!(measured.max_depth >= 2);
    }

    #[test]
    fn mutation_and_testable_context_signals_are_counted() {
        let measured = metrics(
            "fn f(mut value: i32) { value += 1; let closure = || value; let reference = &mut value; unsafe { value += 1; } let _ = closure; let _ = reference; }",
        );

        assert_eq!(measured.mutations, 3);
        assert_eq!(measured.closures, 1);
        assert_eq!(measured.unsafe_blocks, 1);
    }

    #[test]
    fn raw_metrics_have_fixed_structural_counts() {
        let measured = metrics(
            "fn f(mut value: i32, other: i32) { if value > 0 && other > 0 { return; } match value { 0 => {}, _ => {} } for _ in 0..1 { while value > 0 { value -= 1; } } }",
        );

        assert_eq!(measured.parameters, 2);
        assert_eq!(measured.branches, 4);
        assert_eq!(measured.boolean_operators, 1);
        assert_eq!(measured.match_arms, 2);
        assert_eq!(measured.loops, 2);
        assert_eq!(measured.returns, 1);
        assert_eq!(measured.mutations, 1);
        assert_eq!(measured.closures, 0);
        assert_eq!(measured.unsafe_blocks, 0);
        assert_eq!(measured.max_depth, 2);
        assert_eq!(measured.statements, 6);
    }

    #[test]
    fn closure_decisions_keep_the_containing_depth() {
        let measured = metrics(
            "fn f(value: bool) { if value { let check = || if value { true } else { false }; let _ = check; } }",
        );

        assert_eq!(measured.decisions, 2);
        assert_eq!(measured.nesting_penalty, 1);
        assert_eq!(measured.max_depth, 2);
    }

    #[test]
    fn model_one_is_unbounded_and_ignores_non_structural_signals() {
        let measured = Metrics {
            code_lines: 10_000,
            statements: 29,
            decisions: 7,
            nesting_penalty: 11,
            max_depth: 99,
            macro_calls: 13,
            mutations: 100,
            parameters: 100,
            closures: 100,
            unsafe_blocks: 100,
            match_arms: 100,
            ..Metrics::default()
        };

        let scored = score(&measured);
        assert_eq!(scored.value, 20);
        assert_eq!(scored.decisions, 7);
        assert_eq!(scored.nesting_penalty, 11);
        assert_eq!(scored.statement_penalty, 2);
    }

    #[test]
    fn model_two_uses_exact_integer_tenths() {
        let metrics = Metrics {
            control_decisions: 2,
            nesting_penalty: 3,
            boolean_operators: 2,
            statements: 7,
            call_sites: 4,
            explicit_parameters: 3,
            match_arms: 5,
            ..Metrics::default()
        };

        let scored = score_v2(&metrics);
        assert_eq!(scored.value, 10 + 20 + 30 + 10 + 7 + 8 + 6 + 10);
        assert_eq!(scored.units, scored.value);
        assert_eq!(scored.display, "10.1");
        assert_eq!(scored.decisions, 2);
    }

    #[test]
    fn exclusive_measurement_keeps_closure_body_out_of_parent() {
        let item: syn::ItemFn = syn::parse_str(
            "fn f(value: bool) { if value { let check = || if value { call(); } else { other(); }; check(); } }",
        )
        .unwrap();
        let inclusive = measure(&item.block, 1, 1);
        let exclusive = measure_exclusive(&item.block, 1, 1, 1);

        assert!(inclusive.control_decisions > exclusive.control_decisions);
        assert!(inclusive.call_sites > exclusive.call_sites);
        assert_eq!(exclusive.control_decisions, 1);
        assert_eq!(exclusive.call_sites, 1);
    }

    #[test]
    fn ordinary_and_method_calls_are_counted() {
        let measured = metrics(
            "fn f(value: Vec<i32>) { consume(value.clone()); value.len(); value.push(1); }",
        );
        assert_eq!(measured.call_sites, 4);
    }

    #[test]
    fn expression_operations_count_only_the_v3_nodes() {
        let measured = metrics(
            "fn f(mut value: i32, index: usize, flag: bool) { let record = value; let field = record.abs; let borrowed = &value; let grouped = (value); let indexed = values[index]; let casted = value as i64; let combined = value + 1; let short = flag && flag || flag; let called = call(value); let dereferenced = *borrowed; let negated = -value; let inverted = !flag; value = combined; value += 1; }",
        );

        assert_eq!(measured.expression_operations, 8);
        assert_eq!(score_v3(&measured).expression_operation_units, 8);
        assert_eq!(score_v3(&measured).statement_penalty, 0);
    }

    #[test]
    fn expression_operations_keep_nested_callable_bodies_exclusive() {
        let item: syn::ItemFn =
            syn::parse_str("fn f(value: i32) { let check = || value + 1; let _ = check; }")
                .unwrap();
        let parent = measure_exclusive(&item.block, 1, 1, 1);
        let closure: syn::ExprClosure = syn::parse_str("|| value + 1").unwrap();
        let child = measure_closure(&closure.body, 1, closure.inputs.len());

        assert_eq!(parent.expression_operations, 0);
        assert_eq!(child.expression_operations, 1);
    }
}
