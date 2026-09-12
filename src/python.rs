//! Python source analysis backed by Ruff's parser and AST.
//!
//! This module deliberately lowers Ruff's AST into Kompass's language-neutral
//! report types at the frontend boundary. The rest of the analyzer therefore
//! does not need to know about Python node types, indentation tokens, or Ruff
//! parser lifetimes.

use std::path::Path;

use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{
    BoolOp, CmpOp, Expr, ExprBoolOp, ExprCall, ExprCompare, ExprLambda, Mod, ModModule, Operator,
    Parameters, PySourceType, PythonVersion, Stmt, StmtClassDef, StmtFunctionDef, UnaryOp,
};
use ruff_python_parser::{ParseOptions, parse_unchecked};
use ruff_source_file::UniversalNewlines;
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::model::{
    Category, FileAnalysis, FunctionKind, FunctionReport, LineCounts, Location, MacroOpacity,
    Metrics, Position,
};
use crate::score;

/// Analyze one Python source file using strict syntax validation.
///
/// `path` is accepted so the function can be passed directly to
/// [`crate::analyze::PythonAnalyzer`]. The frontend intentionally leaves path
/// and aggregate metadata to the caller.
pub fn analyze_file(
    _path: &Path,
    source: &str,
    category: Category,
) -> Result<FileAnalysis, String> {
    if source.as_bytes().starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err("UTF-8 BOM is not supported; remove the BOM before analysis".to_owned());
    }

    let options =
        ParseOptions::from(PySourceType::Python).with_target_version(PythonVersion::PY314);
    let parsed = parse_unchecked(source, options);
    if parsed.has_syntax_errors() {
        return Err(format_parse_errors(&parsed));
    }

    let module = parsed
        .syntax()
        .as_module()
        .ok_or_else(|| "Python frontend produced a non-module syntax tree".to_owned())?;
    let line_map = LineMap::new(source, parsed.tokens());
    let tokens = count_tokens(parsed.tokens());
    let mut collector = UnitCollector {
        source,
        tokens: parsed.tokens(),
        line_map: &line_map,
        category,
        scope: Vec::new(),
        functions: Vec::new(),
    };
    collector.collect_module(module);

    Ok(FileAnalysis {
        lines: line_map.counts.clone(),
        tokens,
        functions: collector.functions,
        macro_opacity: MacroOpacity::default(),
    })
}

/// Convenience wrapper for callers that do not have a path.
pub fn analyze(source: &str, category: Category) -> Result<FileAnalysis, String> {
    analyze_file(Path::new("<python>"), source, category)
}

fn format_parse_errors(parsed: &ruff_python_parser::Parsed<Mod>) -> String {
    let mut messages = parsed
        .errors()
        .iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    messages.extend(
        parsed
            .unsupported_syntax_errors()
            .iter()
            .map(|error| error.to_string()),
    );
    if messages.is_empty() {
        "Python source could not be parsed".to_owned()
    } else {
        messages.join("; ")
    }
}

fn count_tokens(tokens: &Tokens) -> usize {
    tokens
        .iter()
        .filter(|token| is_counted_token(token.kind()))
        .count()
}

fn is_counted_token(kind: TokenKind) -> bool {
    !matches!(
        kind,
        TokenKind::Comment
            | TokenKind::Newline
            | TokenKind::NonLogicalNewline
            | TokenKind::Indent
            | TokenKind::Dedent
            | TokenKind::EndOfFile
    )
}

fn count_tokens_in(tokens: &Tokens, range: TextRange) -> usize {
    tokens
        .iter()
        .filter(|token| {
            let token_range = token.range();
            token_range.start() >= range.start()
                && token_range.end() <= range.end()
                && is_counted_token(token.kind())
        })
        .count()
}

#[derive(Clone, Debug)]
struct LineMap {
    starts: Vec<usize>,
    ends: Vec<usize>,
    counts: LineCounts,
    code: Vec<bool>,
}

impl LineMap {
    fn new(source: &str, tokens: &Tokens) -> Self {
        let (starts, ends) = line_boundaries(source);
        let line_count = ends.len();
        let mut code = vec![false; line_count];
        let mut comments = vec![false; line_count];
        for token in tokens {
            let token_range = token.range();
            if token_range.is_empty() {
                continue;
            }
            let start = token_range.start().to_usize();
            let end = token_range.end().to_usize();
            let first = line_index(&starts, start);
            let last = line_index(&starts, end.saturating_sub(1));
            for index in first..=last.min(line_count.saturating_sub(1)) {
                if token.kind() == TokenKind::Comment {
                    comments[index] = true;
                } else if is_counted_token(token.kind()) {
                    code[index] = true;
                }
            }
        }

        // Ruff keeps comments in its token stream today, but retaining this
        // source fallback makes line classification stable if that internal
        // detail changes in a future parser release.
        for index in 0..line_count {
            let content = &source[starts[index]..ends[index]];
            if !code.get(index).copied().unwrap_or(false)
                && !comments.get(index).copied().unwrap_or(false)
                && content.trim_start().starts_with('#')
                && let Some(comment) = comments.get_mut(index)
            {
                *comment = true;
            }
        }

        let mut counts = LineCounts {
            total: line_count,
            ..LineCounts::default()
        };
        for index in 0..line_count {
            if code[index] {
                counts.code += 1;
            } else if comments[index] {
                counts.comments += 1;
            } else {
                counts.blank += 1;
            }
        }
        Self {
            starts,
            ends,
            counts,
            code,
        }
    }

    fn code_lines_in(&self, range: TextRange) -> usize {
        if self.code.is_empty() {
            return 0;
        }
        let first = line_index(&self.starts, range.start().to_usize());
        let end_offset = range.end().to_usize();
        let last = line_index(&self.starts, end_offset.saturating_sub(1));
        self.code[first..=last.min(self.code.len().saturating_sub(1))]
            .iter()
            .filter(|is_code| **is_code)
            .count()
    }

    fn position(&self, offset: TextSize) -> Position {
        let offset = offset.to_usize();
        let line = line_index(&self.starts, offset);
        let line_start = self.starts.get(line).copied().unwrap_or(0);
        Position {
            line: line + 1,
            column: offset.saturating_sub(line_start) + 1,
        }
    }

    fn end_position(&self, offset: TextSize) -> Position {
        let offset = offset.to_usize();
        let line = line_index(&self.starts, offset);
        if let Some(end) = self.ends.get(line)
            && offset > *end
        {
            return Position {
                line: line + 1,
                column: end.saturating_sub(self.starts[line]).saturating_add(1),
            };
        }
        self.position(TextSize::new(u32::try_from(offset).unwrap_or(u32::MAX)))
    }
}

fn line_boundaries(source: &str) -> (Vec<usize>, Vec<usize>) {
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    for line in source.universal_newlines() {
        starts.push(line.start().to_usize());
        ends.push(line.end().to_usize());
    }
    if starts.is_empty() {
        starts.push(0);
    }
    (starts, ends)
}

fn line_index(starts: &[usize], offset: usize) -> usize {
    starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScopeKind {
    Module,
    Class,
    Function,
}

struct UnitCollector<'a> {
    source: &'a str,
    tokens: &'a Tokens,
    line_map: &'a LineMap,
    category: Category,
    scope: Vec<String>,
    functions: Vec<FunctionReport>,
}

impl<'a> UnitCollector<'a> {
    fn collect_module(&mut self, module: &ModModule) {
        let range = TextRange::new(TextSize::new(0), text_size(self.source.len()));
        let metrics = measure_body(&module.body, self.line_map.code_lines_in(range), 0, 0, 0);
        if initializer_has_work(&module.body, &metrics) {
            self.functions.push(self.make_report(
                self.qualified_name("<module>"),
                FunctionKind::ModuleInitializer,
                range,
                metrics,
            ));
        }
        self.collect_body(&module.body, 0, ScopeKind::Module);
    }

    fn collect_body(&mut self, body: &[Stmt], depth: usize, scope_kind: ScopeKind) {
        let mut visitor = ChildCollector {
            collector: self,
            depth,
            scope_kind,
        };
        visitor.visit_body(body);
    }

    fn add_function(&mut self, node: &StmtFunctionDef, depth: usize, scope_kind: ScopeKind) {
        let name = node.name.to_string();
        let kind = match scope_kind {
            ScopeKind::Class => FunctionKind::Method,
            ScopeKind::Function => FunctionKind::NestedFunction,
            ScopeKind::Module => FunctionKind::Function,
        };
        let explicit_parameters =
            explicit_parameter_count(&node.parameters, kind == FunctionKind::Method, node);
        let range = node.range;
        let metrics = measure_body(
            &node.body,
            self.line_map.code_lines_in(range),
            node.parameters.len(),
            explicit_parameters,
            depth,
        );
        self.scope.push(name.clone());
        let qualified_name = self.qualified_name("");
        self.scope.pop();
        self.functions
            .push(self.make_report(qualified_name, kind, range, metrics));

        self.collect_function_header(node, depth);
        self.scope.push(name);
        self.collect_body(&node.body, depth, ScopeKind::Function);
        self.scope.pop();
    }

    fn add_class(&mut self, node: &StmtClassDef, depth: usize) {
        let name = node.name.to_string();
        let range = node.range;
        let metrics = measure_body(&node.body, self.line_map.code_lines_in(range), 0, 0, depth);
        self.scope.push(name.clone());
        if initializer_has_work(&node.body, &metrics) {
            let qualified_name = self.qualified_name("<class>");
            self.functions.push(self.make_report(
                qualified_name,
                FunctionKind::ClassInitializer,
                range,
                metrics,
            ));
        }
        self.collect_class_header(node, depth);
        self.collect_body(&node.body, depth, ScopeKind::Class);
        self.scope.pop();
    }

    fn add_lambda(&mut self, node: &ExprLambda, depth: usize) {
        let range = node.range;
        let parameters = node.parameters.as_deref();
        let parameter_count = parameters.map_or(0, Parameters::len);
        let metrics = measure_expression(
            &node.body,
            self.line_map.code_lines_in(range),
            parameter_count,
            parameter_count,
            depth,
        );
        let label = format!(
            "<lambda@{}:{}>",
            self.line_map.position(range.start()).line,
            self.line_map.position(range.start()).column
        );
        self.scope.push(label.clone());
        let qualified_name = self.qualified_name("");
        self.scope.pop();
        self.functions
            .push(self.make_report(qualified_name, FunctionKind::Lambda, range, metrics));

        if let Some(parameters) = &node.parameters {
            let mut visitor = ChildCollector {
                collector: self,
                depth,
                scope_kind: ScopeKind::Function,
            };
            visitor.visit_parameters(parameters);
        }
        self.scope.push(label);
        self.collect_expr(&node.body, depth, ScopeKind::Function);
        self.scope.pop();
    }

    fn collect_function_header(&mut self, node: &StmtFunctionDef, depth: usize) {
        for decorator in &node.decorator_list {
            self.collect_expr(&decorator.expression, depth, ScopeKind::Function);
        }
        if let Some(type_params) = &node.type_params {
            let mut visitor = ChildCollector {
                collector: self,
                depth,
                scope_kind: ScopeKind::Function,
            };
            visitor.visit_type_params(type_params);
        }
        let mut visitor = ChildCollector {
            collector: self,
            depth,
            scope_kind: ScopeKind::Function,
        };
        visitor.visit_parameters(&node.parameters);
        if let Some(returns) = &node.returns {
            visitor.visit_expr(returns);
        }
    }

    fn collect_class_header(&mut self, node: &StmtClassDef, depth: usize) {
        for decorator in &node.decorator_list {
            self.collect_expr(&decorator.expression, depth, ScopeKind::Module);
        }
        if let Some(type_params) = &node.type_params {
            let mut visitor = ChildCollector {
                collector: self,
                depth,
                scope_kind: ScopeKind::Module,
            };
            visitor.visit_type_params(type_params);
        }
        if let Some(arguments) = &node.arguments {
            let mut visitor = ChildCollector {
                collector: self,
                depth,
                scope_kind: ScopeKind::Module,
            };
            visitor.visit_arguments(arguments);
        }
    }

    fn collect_expr(&mut self, expression: &Expr, depth: usize, scope_kind: ScopeKind) {
        let mut visitor = ChildCollector {
            collector: self,
            depth,
            scope_kind,
        };
        visitor.visit_expr(expression);
    }

    fn make_report(
        &self,
        name: String,
        kind: FunctionKind,
        range: TextRange,
        metrics: Metrics,
    ) -> FunctionReport {
        let start = self.line_map.position(range.start());
        let end = self.line_map.end_position(range.end());
        let lines = end.line.saturating_sub(start.line) + 1;
        FunctionReport {
            name,
            kind,
            category: self.category,
            location: Location { start, end },
            lines,
            tokens: count_tokens_in(self.tokens, range),
            score: score::score(&metrics),
            metrics,
        }
    }

    fn qualified_name(&self, leaf: &str) -> String {
        let mut parts = self.scope.clone();
        if !leaf.is_empty() {
            parts.push(leaf.to_owned());
        }
        if parts.is_empty() {
            leaf.to_owned()
        } else {
            parts.join("::")
        }
    }
}

fn initializer_has_work(body: &[Stmt], metrics: &Metrics) -> bool {
    score::score(metrics).value > 10 || body.iter().any(statement_runs_directly)
}

fn statement_runs_directly(statement: &Stmt) -> bool {
    match statement {
        Stmt::FunctionDef(_) | Stmt::ClassDef(_) | Stmt::Pass(_) => false,
        Stmt::Expr(statement) => !matches!(statement.value.as_ref(), Expr::StringLiteral(_)),
        _ => true,
    }
}

struct ChildCollector<'a, 'b> {
    collector: &'b mut UnitCollector<'a>,
    depth: usize,
    scope_kind: ScopeKind,
}

impl<'ast, 'a, 'b> Visitor<'ast> for ChildCollector<'a, 'b> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(node) => {
                self.collector
                    .add_function(node, self.depth, self.scope_kind)
            }
            Stmt::ClassDef(node) => self.collector.add_class(node, self.depth),
            Stmt::If(node) => {
                self.visit_expr(&node.test);
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                for clause in &node.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.visit_expr(test);
                    }
                    self.with_depth(|visitor| visitor.visit_body(&clause.body));
                }
            }
            Stmt::For(node) => {
                self.visit_expr(&node.iter);
                self.visit_expr(&node.target);
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                self.with_depth(|visitor| visitor.visit_body(&node.orelse));
            }
            Stmt::While(node) => {
                self.visit_expr(&node.test);
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                self.with_depth(|visitor| visitor.visit_body(&node.orelse));
            }
            Stmt::With(node) => {
                for item in &node.items {
                    self.visit_with_item(item);
                }
                self.visit_body(&node.body);
            }
            Stmt::Match(node) => {
                self.visit_expr(&node.subject);
                for case in &node.cases {
                    self.with_depth(|visitor| visitor.visit_match_case(case));
                }
            }
            Stmt::Try(node) => {
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                for handler in &node.handlers {
                    self.with_depth(|visitor| visitor.visit_except_handler(handler));
                }
                self.with_depth(|visitor| visitor.visit_body(&node.orelse));
                self.with_depth(|visitor| visitor.visit_body(&node.finalbody));
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Lambda(node) => self.collector.add_lambda(node, self.depth),
            Expr::If(node) => {
                self.visit_expr(&node.test);
                self.with_depth(|visitor| visitor.visit_expr(&node.body));
                self.with_depth(|visitor| visitor.visit_expr(&node.orelse));
            }
            Expr::ListComp(node) => self.visit_comprehension_expr(&node.generators, |visitor| {
                visitor.visit_expr(&node.elt)
            }),
            Expr::SetComp(node) => self.visit_comprehension_expr(&node.generators, |visitor| {
                visitor.visit_expr(&node.elt)
            }),
            Expr::DictComp(node) => self.visit_comprehension_expr(&node.generators, |visitor| {
                if let Some(key) = &node.key {
                    visitor.visit_expr(key);
                }
                visitor.visit_expr(&node.value);
            }),
            Expr::Generator(node) => self.visit_comprehension_expr(&node.generators, |visitor| {
                visitor.visit_expr(&node.elt)
            }),
            _ => visitor::walk_expr(self, expr),
        }
    }

    fn visit_comprehension(&mut self, comprehension: &'ast ruff_python_ast::Comprehension) {
        self.visit_expr(&comprehension.iter);
        self.visit_expr(&comprehension.target);
        for condition in &comprehension.ifs {
            self.visit_expr(condition);
        }
    }
}

impl ChildCollector<'_, '_> {
    fn with_depth(&mut self, visit: impl FnOnce(&mut Self)) {
        self.depth += 1;
        visit(self);
        self.depth -= 1;
    }

    fn visit_comprehension_expr(
        &mut self,
        generators: &[ruff_python_ast::Comprehension],
        element: impl FnOnce(&mut Self),
    ) {
        for generator in generators {
            self.visit_expr(&generator.iter);
            self.visit_expr(&generator.target);
            self.depth += 1;
            for condition in &generator.ifs {
                self.visit_expr(condition);
            }
        }
        element(self);
        self.depth = self.depth.saturating_sub(generators.len());
    }
}

fn explicit_parameter_count(
    parameters: &Parameters,
    is_method: bool,
    function: &StmtFunctionDef,
) -> usize {
    let mut count = parameters.len();
    if is_method
        && !is_staticmethod(function)
        && let Some(first) = parameters.iter().next()
        && matches!(first.name().as_ref(), "self" | "cls")
    {
        count = count.saturating_sub(1);
    }
    count
}

fn is_staticmethod(function: &StmtFunctionDef) -> bool {
    function
        .decorator_list
        .iter()
        .any(|decorator| match &decorator.expression {
            Expr::Name(name) => name.id.as_ref() == "staticmethod",
            Expr::Attribute(attribute) => attribute.attr.as_ref() == "staticmethod",
            _ => false,
        })
}

fn text_size(value: usize) -> TextSize {
    TextSize::new(u32::try_from(value).unwrap_or(u32::MAX))
}

fn measure_body(
    body: &[Stmt],
    code_lines: usize,
    parameters: usize,
    explicit_parameters: usize,
    base_depth: usize,
) -> Metrics {
    let mut visitor = MetricsCollector {
        metrics: Metrics {
            code_lines,
            parameters,
            explicit_parameters,
            ..Metrics::default()
        },
        depth: base_depth,
    };
    visitor.visit_body(body);
    visitor.metrics
}

fn measure_expression(
    expression: &Expr,
    code_lines: usize,
    parameters: usize,
    explicit_parameters: usize,
    base_depth: usize,
) -> Metrics {
    let mut visitor = MetricsCollector {
        metrics: Metrics {
            code_lines,
            parameters,
            explicit_parameters,
            ..Metrics::default()
        },
        depth: base_depth,
    };
    visitor.visit_expr(expression);
    visitor.metrics.statements = 1;
    visitor.metrics
}

struct MetricsCollector {
    metrics: Metrics,
    depth: usize,
}

impl MetricsCollector {
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

    fn visit_function_header(&mut self, node: &StmtFunctionDef) {
        for decorator in &node.decorator_list {
            self.visit_decorator(decorator);
        }
        if let Some(type_params) = &node.type_params {
            self.visit_type_params(type_params);
        }
        self.visit_parameters(&node.parameters);
        if let Some(returns) = &node.returns {
            self.visit_expr(returns);
        }
    }

    fn visit_class_header(&mut self, node: &StmtClassDef) {
        for decorator in &node.decorator_list {
            self.visit_decorator(decorator);
        }
        if let Some(type_params) = &node.type_params {
            self.visit_type_params(type_params);
        }
        if let Some(arguments) = &node.arguments {
            self.visit_arguments(arguments);
        }
    }

    fn visit_comprehension_expr(
        &mut self,
        generators: &[ruff_python_ast::Comprehension],
        element: impl FnOnce(&mut Self),
    ) {
        for generator in generators {
            self.branch();
            self.metrics.loops += 1;
            self.visit_expr(&generator.iter);
            self.visit_expr(&generator.target);
            self.depth += 1;
            for condition in &generator.ifs {
                self.branch();
                self.visit_expr(condition);
            }
        }
        element(self);
        self.depth = self.depth.saturating_sub(generators.len());
    }
}

impl<'ast> Visitor<'ast> for MetricsCollector {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        self.metrics.statements += 1;
        match stmt {
            Stmt::FunctionDef(node) => self.visit_function_header(node),
            Stmt::ClassDef(node) => self.visit_class_header(node),
            Stmt::If(node) => {
                self.branch();
                self.visit_expr(&node.test);
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                for clause in &node.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.branch();
                        self.visit_expr(test);
                        self.with_depth(|visitor| visitor.visit_body(&clause.body));
                    } else {
                        self.with_depth(|visitor| visitor.visit_body(&clause.body));
                    }
                }
            }
            Stmt::For(node) => {
                self.branch();
                self.metrics.loops += 1;
                self.visit_expr(&node.iter);
                self.visit_expr(&node.target);
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                self.with_depth(|visitor| visitor.visit_body(&node.orelse));
            }
            Stmt::While(node) => {
                self.branch();
                self.metrics.loops += 1;
                self.visit_expr(&node.test);
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                self.with_depth(|visitor| visitor.visit_body(&node.orelse));
            }
            Stmt::With(node) => {
                self.metrics.expression_operations += node.items.len();
                for item in &node.items {
                    self.visit_with_item(item);
                }
                self.visit_body(&node.body);
            }
            Stmt::Match(node) => {
                self.branch();
                self.metrics.match_arms += node.cases.len();
                self.visit_expr(&node.subject);
                for case in &node.cases {
                    self.with_depth(|visitor| {
                        visitor.visit_pattern(&case.pattern);
                        if let Some(guard) = &case.guard {
                            visitor.branch();
                            visitor.visit_expr(guard);
                        }
                        visitor.visit_body(&case.body);
                    });
                }
            }
            Stmt::Try(node) => {
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                for handler in &node.handlers {
                    self.branch();
                    match handler {
                        ruff_python_ast::ExceptHandler::ExceptHandler(handler) => {
                            if let Some(type_) = &handler.type_ {
                                self.visit_expr(type_);
                            }
                            self.with_depth(|visitor| visitor.visit_body(&handler.body));
                        }
                    }
                }
                self.with_depth(|visitor| visitor.visit_body(&node.orelse));
                self.with_depth(|visitor| visitor.visit_body(&node.finalbody));
            }
            Stmt::Assign(node) => {
                self.metrics.mutations += 1;
                self.metrics.expression_operations += 1;
                self.visit_expr(&node.value);
                for target in &node.targets {
                    self.visit_expr(target);
                }
            }
            Stmt::AugAssign(node) => {
                self.metrics.mutations += 1;
                self.metrics.expression_operations += 1;
                self.visit_expr(&node.value);
                self.visit_expr(&node.target);
            }
            Stmt::AnnAssign(node) => {
                self.metrics.mutations += 1;
                self.metrics.expression_operations += 1;
                self.visit_expr(&node.annotation);
                self.visit_expr(&node.target);
                if let Some(value) = &node.value {
                    self.visit_expr(value);
                }
            }
            Stmt::TypeAlias(node) => {
                self.metrics.mutations += 1;
                self.metrics.expression_operations += 1;
                self.visit_expr(&node.name);
                if let Some(type_params) = &node.type_params {
                    self.visit_type_params(type_params);
                }
                self.visit_expr(&node.value);
            }
            Stmt::Delete(node) => {
                self.metrics.mutations += node.targets.len();
                for target in &node.targets {
                    self.visit_expr(target);
                }
            }
            Stmt::Return(node) => {
                self.metrics.returns += 1;
                if let Some(value) = &node.value {
                    self.visit_expr(value);
                }
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::BoolOp(ExprBoolOp { values, .. }) => {
                let links = values.len().saturating_sub(1);
                self.metrics.boolean_operators += links;
                self.metrics.decisions += links;
                for value in values {
                    self.visit_expr(value);
                }
            }
            Expr::BinOp(node) => {
                self.metrics.expression_operations += 1;
                self.visit_expr(&node.left);
                self.visit_expr(&node.right);
            }
            Expr::UnaryOp(node) => {
                self.metrics.expression_operations += 1;
                self.visit_expr(&node.operand);
            }
            Expr::Compare(ExprCompare {
                ops,
                left,
                comparators,
                ..
            }) => {
                self.metrics.expression_operations += ops.len();
                self.visit_expr(left);
                for comparator in comparators {
                    self.visit_expr(comparator);
                }
            }
            Expr::Call(ExprCall {
                func, arguments, ..
            }) => {
                self.metrics.call_sites += 1;
                self.visit_expr(func);
                self.visit_arguments(arguments);
            }
            Expr::Subscript(node) => {
                self.metrics.expression_operations += 1;
                self.visit_expr(&node.value);
                self.visit_expr(&node.slice);
            }
            Expr::Await(node) => {
                self.metrics.expression_operations += 1;
                self.visit_expr(&node.value);
            }
            Expr::Yield(node) => {
                self.metrics.expression_operations += 1;
                if let Some(value) = &node.value {
                    self.visit_expr(value);
                }
            }
            Expr::YieldFrom(node) => {
                self.metrics.expression_operations += 1;
                self.visit_expr(&node.value);
            }
            Expr::Named(node) => {
                self.metrics.expression_operations += 1;
                self.metrics.mutations += 1;
                self.visit_expr(&node.value);
                self.visit_expr(&node.target);
            }
            Expr::If(node) => {
                self.branch();
                self.visit_expr(&node.test);
                self.with_depth(|visitor| visitor.visit_expr(&node.body));
                self.with_depth(|visitor| visitor.visit_expr(&node.orelse));
            }
            Expr::Lambda(node) => {
                self.metrics.closures += 1;
                if let Some(parameters) = &node.parameters {
                    self.visit_parameters(parameters);
                }
            }
            Expr::ListComp(node) => self.visit_comprehension_expr(&node.generators, |visitor| {
                visitor.visit_expr(&node.elt)
            }),
            Expr::SetComp(node) => self.visit_comprehension_expr(&node.generators, |visitor| {
                visitor.visit_expr(&node.elt)
            }),
            Expr::DictComp(node) => self.visit_comprehension_expr(&node.generators, |visitor| {
                if let Some(key) = &node.key {
                    visitor.visit_expr(key);
                }
                visitor.visit_expr(&node.value);
            }),
            Expr::Generator(node) => self.visit_comprehension_expr(&node.generators, |visitor| {
                visitor.visit_expr(&node.elt)
            }),
            _ => visitor::walk_expr(self, expr),
        }
    }

    fn visit_bool_op(&mut self, _bool_op: &'ast BoolOp) {}

    fn visit_operator(&mut self, _operator: &'ast Operator) {}

    fn visit_unary_op(&mut self, _unary_op: &'ast UnaryOp) {}

    fn visit_cmp_op(&mut self, _cmp_op: &'ast CmpOp) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_source(source: &str) -> FileAnalysis {
        analyze(source, Category::Production).expect("valid Python fixture")
    }

    #[test]
    fn collects_module_class_method_nested_and_lambda_units() {
        let source = r#""""module docs"""
class Box(Base(make())):
    """class docs"""
    kind = "box"
    @staticmethod
    def build(value, /, *, flag=False):
        if flag:
            nested = lambda item: item + value
        def inner(item):
            return item if flag else value
        return nested(value)
"#;
        let file = analyze_source(source);
        let names = file
            .functions
            .iter()
            .map(|function| (function.name.clone(), function.kind.clone()))
            .collect::<Vec<_>>();
        assert!(
            names
                .iter()
                .any(|(_, kind)| *kind == FunctionKind::ModuleInitializer)
        );
        assert!(
            names
                .iter()
                .any(|(_, kind)| *kind == FunctionKind::ClassInitializer)
        );
        assert!(
            names
                .iter()
                .any(|(name, kind)| name.ends_with("Box::build") && *kind == FunctionKind::Method)
        );
        assert!(
            names
                .iter()
                .any(|(name, kind)| name.ends_with("Box::build::inner")
                    && *kind == FunctionKind::NestedFunction)
        );
        assert!(names.iter().any(|(_, kind)| *kind == FunctionKind::Lambda));
        let method = file
            .functions
            .iter()
            .find(|function| function.name.ends_with("Box::build"))
            .unwrap();
        assert_eq!(method.metrics.parameters, 2);
        assert_eq!(method.metrics.explicit_parameters, 2);
        assert!(method.metrics.control_decisions >= 1);
    }

    #[test]
    fn declaration_only_modules_and_classes_add_no_initializer_tax() {
        let file = analyze_source(
            "\"\"\"docs\"\"\"\nclass C:\n    def run(self):\n        pass\n\ndef helper():\n    pass\n",
        );
        assert!(file.functions.iter().all(|function| {
            !matches!(
                function.kind,
                FunctionKind::ModuleInitializer | FunctionKind::ClassInitializer
            )
        }));
        assert_eq!(file.functions.len(), 2);
    }

    #[test]
    fn lambda_defaults_keep_nested_callable_bodies_visible() {
        let file = analyze_source("value = lambda x=(lambda y: y if y else 0): x\n");
        let lambdas = file
            .functions
            .iter()
            .filter(|function| function.kind == FunctionKind::Lambda)
            .collect::<Vec<_>>();
        assert_eq!(lambdas.len(), 2);
        assert!(
            lambdas
                .iter()
                .any(|function| function.metrics.control_decisions == 1)
        );
    }

    #[test]
    fn nested_callables_keep_their_lexical_control_depth() {
        let file = analyze_source(
            "if enabled:\n    def choose(value):\n        return 1 if value else 0\n",
        );
        let function = file
            .functions
            .iter()
            .find(|function| function.name == "choose")
            .unwrap();
        assert_eq!(function.metrics.control_decisions, 1);
        assert_eq!(function.metrics.nesting_penalty, 1);
    }

    #[test]
    fn method_receiver_is_excluded_but_staticmethod_receiver_is_not() {
        let file = analyze_source(
            "class C:\n    def run(self, value):\n        return value\n    @staticmethod\n    def make(self, value):\n        return value\n",
        );
        let run = file
            .functions
            .iter()
            .find(|function| function.name.ends_with("::run"))
            .unwrap();
        let make = file
            .functions
            .iter()
            .find(|function| function.name.ends_with("::make"))
            .unwrap();
        assert_eq!(run.metrics.parameters, 2);
        assert_eq!(run.metrics.explicit_parameters, 1);
        assert_eq!(make.metrics.parameters, 2);
        assert_eq!(make.metrics.explicit_parameters, 2);
    }

    #[test]
    fn counts_structural_python_signals() {
        let source = r#"def f(items, value):
    try:
        result = [x + 1 for x in items if x > 0]
        if value and value > 1:
            value += 1
    except ValueError:
        del result
    with open("x") as stream:
        return await call(stream[value])
"#;
        let file = analyze_source(source);
        let function = file
            .functions
            .iter()
            .find(|function| function.name == "f")
            .unwrap();
        assert!(function.metrics.control_decisions >= 4);
        assert_eq!(function.metrics.boolean_operators, 1);
        assert!(function.metrics.loops >= 1);
        assert!(function.metrics.expression_operations >= 7);
        assert!(function.metrics.call_sites >= 2);
        assert!(function.metrics.mutations >= 3);
    }

    #[test]
    fn spans_are_utf8_byte_columns_and_tokens_exclude_layout() {
        let source = "# heading\n\ndef café(value):\n    return value + 1  # tail\n";
        let file = analyze_source(source);
        let function = file
            .functions
            .iter()
            .find(|function| function.name == "café")
            .unwrap();
        assert_eq!(function.location.start.line, 3);
        assert_eq!(function.location.start.column, 1);
        assert!(function.tokens > 0);
        assert_eq!(file.lines.comments, 1);
        assert_eq!(file.lines.blank, 1);
        assert_eq!(file.lines.code, 2);
    }

    #[test]
    fn module_locations_stop_before_a_trailing_newline() {
        let file = analyze_source("value = 1\n");
        let module = file
            .functions
            .iter()
            .find(|function| function.kind == FunctionKind::ModuleInitializer)
            .unwrap();
        assert_eq!(file.lines.total, 1);
        assert_eq!(module.location.end.line, 1);
        assert_eq!(module.lines, 1);
    }

    #[test]
    fn universal_newlines_have_real_line_locations() {
        for source in ["def f():\r    return 1\r", "def f():\r\n    return 1\r\n"] {
            let file = analyze_source(source);
            let function = file
                .functions
                .iter()
                .find(|function| function.name == "f")
                .unwrap();
            assert_eq!(file.lines.total, 2);
            assert_eq!(function.location.start.line, 1);
            assert_eq!(function.location.end.line, 2);
            assert_eq!(function.lines, 2);
        }
    }

    #[test]
    fn strict_errors_and_bom_are_reported() {
        assert!(analyze("def broken(:\n    pass\n", Category::Production).is_err());
        assert!(
            analyze("\u{feff}def f():\n    pass\n", Category::Production)
                .unwrap_err()
                .contains("BOM")
        );
    }
}
