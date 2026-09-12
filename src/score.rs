use syn::visit::{self, Visit};
use syn::{
    Arm, BinOp, Expr, ExprAssign, ExprBinary, ExprCast, ExprIf, ExprIndex, ExprMatch,
    ExprReference, ExprUnary, ExprUnsafe, Stmt, UnOp,
};

use crate::model::{Metrics, Score};

/// Measure the structural signals that are available from a Rust syntax
/// tree without compiling or expanding the crate.
pub fn measure(
    block: &syn::Block,
    code_lines: usize,
    parameters: usize,
    explicit_parameters: usize,
) -> Metrics {
    let mut visitor = MetricsVisitor::default();
    visitor.metrics.code_lines = code_lines;
    visitor.metrics.parameters = parameters;
    visitor.metrics.explicit_parameters = explicit_parameters;
    visitor.visit_block(block);
    visitor.metrics
}

/// Measure a closure body as its own callable. A closure body can be either a
/// block or a single expression; the latter is one executable statement for
/// parity with an equivalent function body.
pub fn measure_closure(body: &Expr, code_lines: usize, explicit_parameters: usize) -> Metrics {
    let mut visitor = MetricsVisitor::default();
    visitor.metrics.code_lines = code_lines;
    visitor.metrics.parameters = explicit_parameters;
    visitor.metrics.explicit_parameters = explicit_parameters;
    if !matches!(body, Expr::Block(_)) {
        visitor.metrics.statements = 1;
    }
    visitor.visit_expr(body);
    visitor.metrics
}

/// Score a function using Kompass's current structural model. Scores are
/// exact integer tenths so JSON and downstream tooling never lose precision;
/// text output renders those units as a one-decimal value.
pub fn score(metrics: &Metrics) -> Score {
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
        .saturating_add(metrics.expression_operations)
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
        boolean_operator_units,
        call_site_units,
        parameter_units,
        match_arm_units,
        expression_operation_units: metrics.expression_operations,
    }
}

#[derive(Default)]
struct MetricsVisitor {
    metrics: Metrics,
    depth: usize,
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

    fn visit_expr_closure(&mut self, _node: &'ast syn::ExprClosure) {
        self.metrics.closures += 1;
        // Closures are measured as independent callables. Their bodies are
        // collected and scored separately, so visiting them here would double
        // count their structural signals in the containing function.
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
        measure(&item.block, 1, item.sig.inputs.len(), item.sig.inputs.len())
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

        assert_eq!(measured.decisions, 1);
        assert_eq!(measured.nesting_penalty, 0);
        assert_eq!(measured.max_depth, 1);
    }

    #[test]
    fn measurement_keeps_closure_body_out_of_parent() {
        let item: syn::ItemFn = syn::parse_str(
            "fn f(value: bool) { if value { let check = || if value { call(); } else { other(); }; check(); } }",
        )
        .unwrap();
        let measured = measure(&item.block, 1, 1, 1);

        assert_eq!(measured.control_decisions, 1);
        assert_eq!(measured.call_sites, 1);
    }

    #[test]
    fn ordinary_and_method_calls_are_counted() {
        let measured = metrics(
            "fn f(value: Vec<i32>) { consume(value.clone()); value.len(); value.push(1); }",
        );
        assert_eq!(measured.call_sites, 4);
    }

    #[test]
    fn expression_operations_count_only_the_scored_nodes() {
        let measured = metrics(
            "fn f(mut value: i32, index: usize, flag: bool) { let record = value; let field = record.abs; let borrowed = &value; let grouped = (value); let indexed = values[index]; let casted = value as i64; let combined = value + 1; let short = flag && flag || flag; let called = call(value); let dereferenced = *borrowed; let negated = -value; let inverted = !flag; value = combined; value += 1; }",
        );

        assert_eq!(measured.expression_operations, 8);
        assert_eq!(score(&measured).expression_operation_units, 8);
    }

    #[test]
    fn expression_operations_keep_nested_callable_bodies_exclusive() {
        let item: syn::ItemFn =
            syn::parse_str("fn f(value: i32) { let check = || value + 1; let _ = check; }")
                .unwrap();
        let parent = measure(&item.block, 1, 1, 1);
        let closure: syn::ExprClosure = syn::parse_str("|| value + 1").unwrap();
        let child = measure_closure(&closure.body, 1, closure.inputs.len());

        assert_eq!(parent.expression_operations, 0);
        assert_eq!(child.expression_operations, 1);
    }
}
