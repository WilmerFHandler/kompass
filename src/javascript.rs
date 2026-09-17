//! JavaScript, JSX, TypeScript, and TSX analysis backed by Oxc.
//!
//! The frontend deliberately uses Oxc's `VisitJs` visitor. That visitor keeps
//! runtime expressions in TypeScript while pruning type grammar, which makes
//! the structural score describe emitted JavaScript rather than annotations.

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{VisitJs, walk_js};
use oxc_parser::config::TokensParserConfig;
use oxc_parser::{Kind, ParseOptions, Parser, Token};
use oxc_span::{GetSpan, SourceType, Span};
use oxc_syntax::scope::ScopeFlags;

use crate::identity;
use crate::model::{
    Category, FileAnalysis, FunctionKind, FunctionReport, Language, LineCounts, Location,
    MacroOpacity, Metrics, Position, ScoreComponent, ScoreContribution,
};
use crate::score;

/// Oxc version pinned by the Cargo manifest and embedded in the frontend
/// contract. Keeping this explicit makes parser changes reviewable.
pub const OXC_VERSION: &str = "0.143.0";

/// The four source modes exposed by the frontend. Extensions are intentionally
/// mapped explicitly so a `.js` file cannot silently acquire JSX grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceMode {
    JavaScript,
    JavaScriptJsx,
    TypeScript,
    TypeScriptJsx,
}

impl SourceMode {
    /// Infer a mode from one of the four supported frontend extensions.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("js") => Ok(Self::JavaScript),
            Some("jsx") => Ok(Self::JavaScriptJsx),
            Some("ts") => Ok(Self::TypeScript),
            Some("tsx") => Ok(Self::TypeScriptJsx),
            Some(extension) => Err(format!(
                "unsupported JavaScript frontend extension .{extension}; expected .js, .jsx, .ts, or .tsx"
            )),
            None => Err(
                "JavaScript frontend requires a .js, .jsx, .ts, or .tsx file extension".to_owned(),
            ),
        }
    }

    const fn source_type(self) -> SourceType {
        match self {
            Self::JavaScript => SourceType::unambiguous(),
            Self::JavaScriptJsx => SourceType::unambiguous().with_jsx(true),
            Self::TypeScript => SourceType::ts(),
            Self::TypeScriptJsx => SourceType::tsx(),
        }
    }

    pub const fn serialized(self) -> &'static str {
        match self {
            Self::JavaScript => "javascript",
            Self::JavaScriptJsx => "javascript-jsx",
            Self::TypeScript => "typescript",
            Self::TypeScriptJsx => "typescript-jsx",
        }
    }
}

/// Analyze a source file after inferring its explicit mode from the path.
pub fn analyze_file(path: &Path, source: &str, category: Category) -> Result<FileAnalysis, String> {
    let mode = SourceMode::from_path(path)?;
    analyze_source(source, mode, category)
}

/// Analyze source with an explicitly selected mode.
pub fn analyze_source(
    source: &str,
    mode: SourceMode,
    category: Category,
) -> Result<FileAnalysis, String> {
    if source.as_bytes().starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err("UTF-8 BOM is not supported; remove the BOM before analysis".to_owned());
    }

    let allocator = Allocator::default();
    let options = ParseOptions {
        parse_regular_expression: true,
        ..ParseOptions::default()
    };
    let parsed = Parser::new(&allocator, source, mode.source_type())
        .with_config(TokensParserConfig)
        .with_options(options)
        .parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        return Err(format_parse_errors(&parsed.diagnostics));
    }

    let token_index = TokenIndex::new(&parsed.tokens);
    let line_map = LineMap::new(source, &token_index);
    let mut functions = Vec::new();
    let mut walker = DiscoveryWalker {
        source,
        token_index: &token_index,
        line_map: &line_map,
        category,
        scope: Vec::new(),
        depth: 0,
        in_function: false,
        functions: &mut functions,
    };
    walker.visit_program(&parsed.program);
    let evidence =
        extract_javascript_evidence(source, &token_index, &line_map, &parsed.program, &functions);

    Ok(FileAnalysis {
        lines: line_map.counts.clone(),
        tokens: token_index.total(),
        functions,
        macro_opacity: MacroOpacity::default(),
        evidence,
    })
}

/// Convenience wrapper for callers without a path.
pub fn analyze(source: &str, mode: SourceMode, category: Category) -> Result<FileAnalysis, String> {
    analyze_source(source, mode, category)
}

fn format_parse_errors(diagnostics: &oxc_diagnostics::Diagnostics) -> String {
    diagnostics
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Clone, Copy, Debug)]
struct TokenRange {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, Default)]
struct TokenIndex {
    ranges: Vec<TokenRange>,
}

impl TokenIndex {
    fn new(tokens: &[Token]) -> Self {
        let ranges = tokens
            .iter()
            .filter(|token| token.kind() != Kind::Skip && !token.kind().is_eof())
            .map(|token| TokenRange {
                start: token.start() as usize,
                end: token.end() as usize,
            })
            .collect();
        Self { ranges }
    }

    fn total(&self) -> usize {
        self.ranges.len()
    }

    fn count_in(&self, span: Span) -> usize {
        let start = span.start as usize;
        let end = span.end as usize;
        self.ranges
            .iter()
            .filter(|range| range.start >= start && range.end <= end)
            .count()
    }

    fn text_in(&self, source: &str, span: Span) -> Vec<String> {
        let start = span.start as usize;
        let end = span.end as usize;
        self.ranges
            .iter()
            .filter(|range| range.start >= start && range.end <= end)
            .filter_map(|range| source.get(range.start..range.end))
            .map(str::to_owned)
            .collect()
    }

    fn mark_lines(&self, starts: &[usize], code: &mut [bool]) {
        for range in &self.ranges {
            let first = line_index(starts, range.start);
            let last = line_index(starts, range.end.saturating_sub(1));
            for index in first..=last.min(code.len().saturating_sub(1)) {
                if let Some(mark) = code.get_mut(index) {
                    *mark = true;
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct LineMap {
    counts: LineCounts,
    starts: Vec<usize>,
    code: Vec<bool>,
}

impl LineMap {
    fn new(source: &str, tokens: &TokenIndex) -> Self {
        let starts = line_starts(source);
        let total = if source.is_empty() { 0 } else { starts.len() };
        let mut code = vec![false; total.max(1)];
        tokens.mark_lines(&starts, &mut code);
        let mut comments = vec![false; code.len()];
        mark_comment_lines(source, &starts, &mut comments);
        let blank = if total == 0 {
            0
        } else {
            (0..total)
                .filter(|index| !code[*index] && !comments[*index])
                .count()
        };
        let comment_count = if total == 0 {
            0
        } else {
            (0..total)
                .filter(|index| !code[*index] && comments[*index])
                .count()
        };
        Self {
            counts: LineCounts {
                total,
                code: code.iter().take(total).filter(|value| **value).count(),
                comments: comment_count,
                blank,
            },
            starts,
            code,
        }
    }

    fn position(&self, offset: u32) -> Position {
        let offset = offset as usize;
        let index = line_index(&self.starts, offset);
        let start = self.starts.get(index).copied().unwrap_or(0);
        Position {
            line: index + 1,
            column: offset.saturating_sub(start) + 1,
        }
    }

    fn end_position(&self, offset: u32) -> Position {
        self.position(offset)
    }

    fn code_lines_in(&self, span: Span) -> usize {
        if self.counts.total == 0 {
            return 0;
        }
        let start = line_index(&self.starts, span.start as usize);
        let end_offset = (span.end as usize).saturating_sub(1);
        let end = line_index(&self.starts, end_offset);
        (start..=end.min(self.counts.total.saturating_sub(1)))
            .filter(|index| self.code.get(*index).copied().unwrap_or(false))
            .count()
    }
}

fn line_starts(source: &str) -> Vec<usize> {
    let bytes = source.as_bytes();
    let mut starts = vec![0];
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            if bytes.get(index + 1) == Some(&b'\n') {
                index += 1;
            }
            if index + 1 < bytes.len() {
                starts.push(index + 1);
            }
        } else if bytes[index] == b'\n' && index + 1 < bytes.len() {
            starts.push(index + 1);
        }
        index += 1;
    }
    starts
}

fn line_index(starts: &[usize], offset: usize) -> usize {
    starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1)
}

fn mark_comment_lines(source: &str, starts: &[usize], comments: &mut [bool]) {
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(quote_byte) = quote {
            if byte == b'\\' {
                index = index.saturating_add(2);
                continue;
            }
            if byte == quote_byte {
                quote = None;
            }
            index += 1;
            continue;
        }
        if byte == b'\'' || byte == b'"' || byte == b'`' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            let start = index;
            while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
                index += 1;
            }
            mark_span_lines(starts, comments, start, index);
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            let start = index;
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            mark_span_lines(starts, comments, start, index);
            continue;
        }
        index += 1;
    }
}

fn mark_span_lines(starts: &[usize], marks: &mut [bool], start: usize, end: usize) {
    if starts.is_empty() || marks.is_empty() || start >= end {
        return;
    }
    let first = line_index(starts, start);
    let last = line_index(starts, end.saturating_sub(1));
    for index in first..=last.min(marks.len().saturating_sub(1)) {
        marks[index] = true;
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingContribution {
    component: ScoreComponent,
    units: usize,
    span: Span,
}

fn metrics_for_statements(
    statements: &[Statement<'_>],
    line_map: &LineMap,
    range: Span,
    depth: usize,
    parameters: usize,
) -> (Metrics, Vec<PendingContribution>) {
    let mut visitor = MetricsCollector {
        metrics: Metrics {
            code_lines: line_map.code_lines_in(range),
            parameters,
            explicit_parameters: parameters,
            ..Metrics::default()
        },
        contributions: Vec::new(),
        depth,
    };
    for statement in statements {
        visitor.visit_statement(statement);
    }
    (visitor.metrics, visitor.contributions)
}

fn metrics_for_expression(
    expression: &Expression<'_>,
    line_map: &LineMap,
    range: Span,
    depth: usize,
    parameters: usize,
) -> (Metrics, Vec<PendingContribution>) {
    let mut visitor = MetricsCollector {
        metrics: Metrics {
            code_lines: line_map.code_lines_in(range),
            parameters,
            explicit_parameters: parameters,
            statements: 1,
            ..Metrics::default()
        },
        contributions: Vec::new(),
        depth,
    };
    visitor.visit_expression(expression);
    (visitor.metrics, visitor.contributions)
}

struct MetricsCollector {
    metrics: Metrics,
    contributions: Vec<PendingContribution>,
    depth: usize,
}

impl MetricsCollector {
    fn add(&mut self, component: ScoreComponent, units: usize, span: Span) {
        self.contributions
            .push(PendingContribution::new(component, units, span));
    }

    fn operation(&mut self, span: Span) {
        self.metrics.expression_operations += 1;
        self.add(ScoreComponent::ExpressionOperations, 1, span);
    }

    fn branch(&mut self, span: Span) {
        self.metrics.branches += 1;
        self.metrics.decisions += 1;
        self.metrics.control_decisions += 1;
        self.metrics.nesting_penalty = self.metrics.nesting_penalty.saturating_add(self.depth);
        self.metrics.max_depth = self.metrics.max_depth.max(self.depth + 1);
        self.add(ScoreComponent::ControlDecisions, 10, span);
        if self.depth > 0 {
            self.add(
                ScoreComponent::NestingPenalty,
                self.depth.saturating_mul(10),
                span,
            );
        }
    }

    fn boolean(&mut self, span: Span) {
        self.metrics.boolean_operators += 1;
        self.metrics.decisions += 1;
        self.add(ScoreComponent::BooleanOperators, 5, span);
    }

    fn with_depth<F>(&mut self, function: F)
    where
        F: FnOnce(&mut Self),
    {
        self.depth += 1;
        function(self);
        self.depth -= 1;
    }

    fn visit_statements(&mut self, statements: &[Statement<'_>]) {
        for statement in statements {
            self.visit_statement(statement);
        }
    }

    fn visit_arguments(&mut self, arguments: &[Argument<'_>]) {
        for argument in arguments {
            self.visit_argument(argument);
        }
    }
}

impl<'ast> VisitJs<'ast> for MetricsCollector {
    fn visit_declaration(&mut self, declaration: &Declaration<'ast>) {
        match declaration {
            Declaration::VariableDeclaration(node) => self.visit_variable_declaration(node),
            Declaration::FunctionDeclaration(_)
            | Declaration::ClassDeclaration(_)
            | Declaration::TSTypeAliasDeclaration(_)
            | Declaration::TSInterfaceDeclaration(_)
            | Declaration::TSEnumDeclaration(_)
            | Declaration::TSModuleDeclaration(_)
            | Declaration::TSGlobalDeclaration(_)
            | Declaration::TSImportEqualsDeclaration(_) => {}
        }
    }

    fn visit_statement(&mut self, statement: &Statement<'ast>) {
        let executable = matches!(
            statement,
            Statement::VariableDeclaration(_)
                | Statement::ExpressionStatement(_)
                | Statement::IfStatement(_)
                | Statement::DoWhileStatement(_)
                | Statement::WhileStatement(_)
                | Statement::ForStatement(_)
                | Statement::ForInStatement(_)
                | Statement::ForOfStatement(_)
                | Statement::ReturnStatement(_)
                | Statement::SwitchStatement(_)
                | Statement::ThrowStatement(_)
                | Statement::TryStatement(_)
                | Statement::WithStatement(_)
                | Statement::BreakStatement(_)
                | Statement::ContinueStatement(_)
                | Statement::DebuggerStatement(_)
                | Statement::LabeledStatement(_)
                | Statement::ExportDeclaration(_)
        );
        if executable {
            self.metrics.statements += 1;
        }

        match statement {
            Statement::BlockStatement(block) => self.visit_block_statement(block),
            Statement::IfStatement(node) => self.visit_if_statement(node),
            Statement::DoWhileStatement(node) => self.visit_do_while_statement(node),
            Statement::WhileStatement(node) => self.visit_while_statement(node),
            Statement::ForStatement(node) => self.visit_for_statement(node),
            Statement::ForInStatement(node) => self.visit_for_in_statement(node),
            Statement::ForOfStatement(node) => self.visit_for_of_statement(node),
            Statement::SwitchStatement(node) => self.visit_switch_statement(node),
            Statement::TryStatement(node) => self.visit_try_statement(node),
            Statement::WithStatement(node) => self.visit_with_statement(node),
            Statement::ReturnStatement(node) => self.visit_return_statement(node),
            Statement::ExpressionStatement(node) => self.visit_expression(&node.expression),
            Statement::ThrowStatement(node) => self.visit_expression(&node.argument),
            Statement::LabeledStatement(node) => self.visit_statement(&node.body),
            Statement::VariableDeclaration(node) => self.visit_variable_declaration(node),
            Statement::FunctionDeclaration(_)
            | Statement::ClassDeclaration(_)
            | Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::ExportDefaultDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSModuleDeclaration(_)
            | Statement::TSGlobalDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_)
            | Statement::EmptyStatement(_)
            | Statement::BreakStatement(_)
            | Statement::ContinueStatement(_)
            | Statement::DebuggerStatement(_) => {}
            _ => walk_js::walk_statement(self, statement),
        }
    }

    fn visit_block_statement(&mut self, block: &BlockStatement<'ast>) {
        self.visit_statements(&block.body);
    }

    fn visit_variable_declaration(&mut self, declaration: &VariableDeclaration<'ast>) {
        for declarator in &declaration.declarations {
            if let Some(init) = &declarator.init {
                self.visit_expression(init);
            }
        }
    }

    fn visit_if_statement(&mut self, node: &IfStatement<'ast>) {
        self.branch(node.span);
        self.visit_expression(&node.test);
        self.with_depth(|visitor| visitor.visit_statement(&node.consequent));
        if let Some(alternate) = &node.alternate {
            if matches!(alternate, Statement::IfStatement(_)) {
                self.visit_statement(alternate);
            } else {
                self.with_depth(|visitor| visitor.visit_statement(alternate));
            }
        }
    }

    fn visit_do_while_statement(&mut self, node: &DoWhileStatement<'ast>) {
        self.branch(node.span);
        self.with_depth(|visitor| visitor.visit_statement(&node.body));
        self.visit_expression(&node.test);
    }

    fn visit_while_statement(&mut self, node: &WhileStatement<'ast>) {
        self.branch(node.span);
        self.visit_expression(&node.test);
        self.with_depth(|visitor| visitor.visit_statement(&node.body));
    }

    fn visit_for_statement(&mut self, node: &ForStatement<'ast>) {
        self.branch(node.span);
        if let Some(init) = &node.init {
            self.visit_for_statement_init(init);
        }
        if let Some(test) = &node.test {
            self.visit_expression(test);
        }
        if let Some(update) = &node.update {
            self.visit_expression(update);
        }
        self.with_depth(|visitor| visitor.visit_statement(&node.body));
        self.metrics.loops += 1;
    }

    fn visit_for_in_statement(&mut self, node: &ForInStatement<'ast>) {
        self.branch(node.span);
        self.visit_for_statement_left(&node.left);
        self.visit_expression(&node.right);
        self.with_depth(|visitor| visitor.visit_statement(&node.body));
        self.metrics.loops += 1;
    }

    fn visit_for_of_statement(&mut self, node: &ForOfStatement<'ast>) {
        self.branch(node.span);
        self.visit_for_statement_left(&node.left);
        self.visit_expression(&node.right);
        self.with_depth(|visitor| visitor.visit_statement(&node.body));
        self.metrics.loops += 1;
    }

    fn visit_switch_statement(&mut self, node: &SwitchStatement<'ast>) {
        self.branch(node.span);
        self.metrics.match_arms += node.cases.len();
        for case in &node.cases {
            self.add(ScoreComponent::MatchArms, 2, case.span);
        }
        self.visit_expression(&node.discriminant);
        for case in &node.cases {
            self.with_depth(|visitor| visitor.visit_switch_case(case));
        }
    }

    fn visit_switch_case(&mut self, node: &SwitchCase<'ast>) {
        if let Some(test) = &node.test {
            self.visit_expression(test);
        }
        self.visit_statements(&node.consequent);
    }

    fn visit_try_statement(&mut self, node: &TryStatement<'ast>) {
        self.with_depth(|visitor| visitor.visit_block_statement(&node.block));
        if let Some(handler) = &node.handler {
            self.branch(handler.span);
            self.with_depth(|visitor| visitor.visit_block_statement(&handler.body));
        }
        if let Some(finalizer) = &node.finalizer {
            self.with_depth(|visitor| visitor.visit_block_statement(finalizer));
        }
    }

    fn visit_with_statement(&mut self, node: &WithStatement<'ast>) {
        self.branch(node.span);
        self.visit_expression(&node.object);
        self.with_depth(|visitor| visitor.visit_statement(&node.body));
    }

    fn visit_return_statement(&mut self, node: &ReturnStatement<'ast>) {
        self.metrics.returns += 1;
        if let Some(argument) = &node.argument {
            self.visit_expression(argument);
        }
    }

    fn visit_expression(&mut self, expression: &Expression<'ast>) {
        match expression {
            Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {
                self.metrics.closures += 1;
            }
            Expression::ClassExpression(_) => {}
            Expression::BinaryExpression(node) => {
                self.operation(node.span);
                self.visit_expression(&node.left);
                self.visit_expression(&node.right);
            }
            Expression::LogicalExpression(node) => {
                self.boolean(node.span);
                self.visit_expression(&node.left);
                self.visit_expression(&node.right);
            }
            // Optional chaining short-circuits at runtime. Charge one decision
            // for the complete chain rather than one for every optional link.
            Expression::ChainExpression(node) => {
                self.boolean(node.span);
                walk_js::walk_chain_expression(self, node);
            }
            Expression::ConditionalExpression(node) => {
                self.branch(node.span);
                self.visit_expression(&node.test);
                self.with_depth(|visitor| visitor.visit_expression(&node.consequent));
                self.with_depth(|visitor| visitor.visit_expression(&node.alternate));
            }
            Expression::AssignmentExpression(node) => {
                self.operation(node.span);
                self.metrics.mutations += 1;
                self.visit_assignment_target(&node.left);
                self.visit_expression(&node.right);
            }
            Expression::UnaryExpression(node) => {
                self.operation(node.span);
                self.visit_expression(&node.argument);
            }
            Expression::UpdateExpression(node) => {
                self.operation(node.span);
                self.metrics.mutations += 1;
                self.visit_simple_assignment_target(&node.argument);
            }
            Expression::CallExpression(node) => {
                self.metrics.call_sites += 1;
                self.add(ScoreComponent::CallSites, 2, node.span);
                self.visit_expression(&node.callee);
                self.visit_arguments(&node.arguments);
            }
            Expression::NewExpression(node) => {
                self.metrics.call_sites += 1;
                self.add(ScoreComponent::CallSites, 2, node.span);
                self.visit_expression(&node.callee);
                self.visit_arguments(&node.arguments);
            }
            Expression::ComputedMemberExpression(node) => {
                self.operation(node.span);
                self.visit_expression(&node.object);
                self.visit_expression(&node.expression);
            }
            Expression::PrivateInExpression(node) => {
                self.operation(node.span);
                self.visit_expression(&node.right);
            }
            Expression::SequenceExpression(node) => {
                for _ in 1..node.expressions.len() {
                    self.operation(node.span);
                }
                for expression in &node.expressions {
                    self.visit_expression(expression);
                }
            }
            Expression::TaggedTemplateExpression(node) => {
                self.metrics.call_sites += 1;
                self.add(ScoreComponent::CallSites, 2, node.span);
                walk_js::walk_tagged_template_expression(self, node);
            }
            Expression::ImportExpression(node) => {
                self.metrics.call_sites += 1;
                self.add(ScoreComponent::CallSites, 2, node.span);
                walk_js::walk_import_expression(self, node);
            }
            _ => walk_js::walk_expression(self, expression),
        }
    }

    fn visit_arrow_function_expression(&mut self, _node: &ArrowFunctionExpression<'ast>) {
        self.metrics.closures += 1;
    }

    // Chain elements reach this visitor method directly instead of passing
    // through `visit_expression`, so optional calls need their call-site cost
    // recorded here.
    fn visit_call_expression(&mut self, node: &CallExpression<'ast>) {
        self.metrics.call_sites += 1;
        self.add(ScoreComponent::CallSites, 2, node.span);
        self.visit_expression(&node.callee);
        self.visit_arguments(&node.arguments);
    }

    fn visit_function(&mut self, _node: &Function<'ast>, _flags: ScopeFlags) {
        self.metrics.closures += 1;
    }

    fn visit_class(&mut self, _node: &Class<'ast>) {}

    fn visit_argument(&mut self, argument: &Argument<'ast>) {
        match argument {
            Argument::SpreadElement(node) => self.visit_expression(&node.argument),
            _ => walk_js::walk_argument(self, argument),
        }
    }
}

struct DiscoveryWalker<'a> {
    source: &'a str,
    token_index: &'a TokenIndex,
    line_map: &'a LineMap,
    category: Category,
    scope: Vec<String>,
    depth: usize,
    in_function: bool,
    functions: &'a mut Vec<FunctionReport>,
}

impl DiscoveryWalker<'_> {
    fn collect_module(&mut self, program: &Program<'_>) {
        let full = Span::new(0, self.source.len() as u32);
        let (metrics, pending) = metrics_for_statements(&program.body, self.line_map, full, 0, 0);
        let has_work = program.body.iter().any(module_statement_has_runtime_work);
        if has_work {
            let body = program
                .body
                .first()
                .map(statement_span)
                .and_then(|start| {
                    program
                        .body
                        .last()
                        .map(|last| Span::new(start.start, statement_span(last).end))
                })
                .unwrap_or(full);
            self.add_report(
                self.qualified_name("<module>"),
                FunctionKind::ModuleInitializer,
                full,
                body,
                metrics,
                pending,
            );
        }
    }

    fn qualified_name(&self, leaf: &str) -> String {
        let mut parts = self.scope.clone();
        if !leaf.is_empty() {
            parts.push(leaf.to_owned());
        }
        parts.join("::")
    }

    fn with_depth<F>(&mut self, function: F)
    where
        F: FnOnce(&mut Self),
    {
        self.depth += 1;
        function(self);
        self.depth -= 1;
    }

    fn visit_body(&mut self, body: &[Statement<'_>]) {
        for statement in body {
            self.visit_statement(statement);
        }
    }

    fn callable_label(&self, span: Span, hint: Option<&str>) -> String {
        if let Some(hint) = hint.filter(|hint| !hint.is_empty()) {
            return hint.to_owned();
        }
        let start = self.line_map.position(span.start);
        format!("<closure@{}:{}>", start.line, start.column)
    }

    fn add_report(
        &mut self,
        name: String,
        kind: FunctionKind,
        full: Span,
        body: Span,
        metrics: Metrics,
        mut pending: Vec<PendingContribution>,
    ) {
        pending.push(PendingContribution::new(ScoreComponent::Boundary, 10, full));
        let start = self.line_map.position(full.start);
        let end = self.line_map.end_position(full.end);
        let location = Location {
            start: start.clone(),
            end: end.clone(),
        };
        let contributions = pending
            .into_iter()
            .map(|item| ScoreContribution {
                component: item.component,
                units: item.units,
                location: Location {
                    start: self.line_map.position(item.span.start),
                    end: self.line_map.end_position(item.span.end),
                },
            })
            .collect();
        self.functions.push(FunctionReport {
            snapshot_id: String::new(),
            declaration_fingerprint: identity::range_fingerprint(
                self.source,
                full.start as usize,
                body.start as usize,
                Language::JavaScript,
            ),
            body_fingerprint: identity::range_fingerprint(
                self.source,
                body.start as usize,
                body.end as usize,
                Language::JavaScript,
            ),
            name,
            kind,
            category: self.category,
            location,
            lines: end.line.saturating_sub(start.line) + 1,
            tokens: self.token_index.count_in(full),
            score: score::score_with_contributions(
                &metrics,
                Location { start, end },
                contributions,
            ),
            metrics,
        });
    }

    fn explicit_parameter_count(parameters: &FormalParameters<'_>) -> usize {
        parameters.items.len() + usize::from(parameters.rest.is_some())
    }

    fn parameter_contributions(
        parameters: &FormalParameters<'_>,
        contributions: &mut Vec<PendingContribution>,
    ) {
        for parameter in &parameters.items {
            contributions.push(PendingContribution::new(
                ScoreComponent::ExplicitParameters,
                2,
                parameter.span,
            ));
        }
        if let Some(rest) = &parameters.rest {
            contributions.push(PendingContribution::new(
                ScoreComponent::ExplicitParameters,
                2,
                rest.span,
            ));
        }
    }

    fn collect_function(
        &mut self,
        function: &Function<'_>,
        kind: FunctionKind,
        hint: Option<&str>,
        base_depth: usize,
    ) {
        let Some(body) = function.body.as_deref() else {
            return;
        };
        let full = function.span;
        let parameters = Self::explicit_parameter_count(&function.params);
        let (mut metrics, mut pending) = metrics_for_statements(
            &body.statements,
            self.line_map,
            full,
            base_depth,
            parameters,
        );
        Self::parameter_contributions(&function.params, &mut pending);
        metrics.parameters = parameters;
        metrics.explicit_parameters = parameters;
        let local_name = hint
            .map(str::to_owned)
            .or_else(|| function.id.as_ref().map(|id| id.name.to_string()))
            .unwrap_or_else(|| self.callable_label(full, None));
        let name = self.qualified_name(&local_name);
        let body_span = body.span;
        self.add_report(name.clone(), kind, full, body_span, metrics, pending);

        let mut nested = DiscoveryWalker {
            source: self.source,
            token_index: self.token_index,
            line_map: self.line_map,
            category: self.category,
            scope: {
                let mut scope = self.scope.clone();
                scope.push(local_name);
                scope
            },
            depth: 0,
            in_function: true,
            functions: &mut *self.functions,
        };
        for parameter in &function.params.items {
            if let Some(initializer) = &parameter.initializer {
                nested.visit_expression(initializer);
            }
        }
        if let Some(rest) = &function.params.rest {
            nested.visit_binding_pattern(&rest.rest.argument);
        }
        nested.visit_body(&body.statements);
    }

    fn collect_arrow(&mut self, arrow: &ArrowFunctionExpression<'_>, hint: Option<&str>) {
        let parameters = Self::explicit_parameter_count(&arrow.params);
        let full = arrow.span;
        let (mut metrics, mut pending) = match &arrow.body {
            ArrowFunctionBody::FunctionBody(body) => metrics_for_statements(
                &body.statements,
                self.line_map,
                full,
                self.depth,
                parameters,
            ),
            _ => metrics_for_expression(
                arrow.body.to_expression(),
                self.line_map,
                full,
                self.depth,
                parameters,
            ),
        };
        Self::parameter_contributions(&arrow.params, &mut pending);
        metrics.parameters = parameters;
        metrics.explicit_parameters = parameters;
        let local_name = hint
            .map(str::to_owned)
            .unwrap_or_else(|| self.callable_label(full, None));
        let name = self.qualified_name(&local_name);
        let body_span = match &arrow.body {
            ArrowFunctionBody::FunctionBody(body) => body.span,
            _ => arrow.body.span(),
        };
        self.add_report(
            name.clone(),
            FunctionKind::Closure,
            full,
            body_span,
            metrics,
            pending,
        );
        let mut nested = DiscoveryWalker {
            source: self.source,
            token_index: self.token_index,
            line_map: self.line_map,
            category: self.category,
            scope: {
                let mut scope = self.scope.clone();
                scope.push(local_name);
                scope
            },
            depth: self.depth,
            in_function: true,
            functions: &mut *self.functions,
        };
        match &arrow.body {
            ArrowFunctionBody::FunctionBody(body) => nested.visit_body(&body.statements),
            _ => nested.visit_expression(arrow.body.to_expression()),
        }
    }

    fn collect_class(&mut self, class: &Class<'_>, hint: Option<&str>) {
        let full = class.span;
        let local_name = hint
            .map(str::to_owned)
            .or_else(|| class.id.as_ref().map(|id| id.name.to_string()))
            .unwrap_or_else(|| self.callable_label(full, Some("<class>")));
        let mut metrics = Metrics {
            code_lines: self.line_map.code_lines_in(full),
            ..Metrics::default()
        };
        let mut pending = Vec::new();
        if let Some(super_class) = &class.super_class {
            self.visit_expression(super_class);
            add_expression_metrics(
                super_class,
                &mut metrics,
                &mut pending,
                self.line_map,
                self.depth,
            );
        }
        for decorator in &class.decorators {
            self.visit_expression(&decorator.expression);
            add_expression_metrics(
                &decorator.expression,
                &mut metrics,
                &mut pending,
                self.line_map,
                self.depth,
            );
        }

        for element in &class.body.body {
            match element {
                ClassElement::PropertyDefinition(property) => self.collect_class_property(
                    &property.key,
                    property.computed,
                    &property.decorators,
                    property.value.as_ref(),
                    &mut metrics,
                    &mut pending,
                ),
                ClassElement::AccessorProperty(property) => self.collect_class_property(
                    &property.key,
                    property.computed,
                    &property.decorators,
                    property.value.as_ref(),
                    &mut metrics,
                    &mut pending,
                ),
                ClassElement::StaticBlock(block) => {
                    metrics.statements += block.body.len();
                    let (block_metrics, block_pending) = metrics_for_statements(
                        &block.body,
                        self.line_map,
                        block.span,
                        self.depth,
                        0,
                    );
                    merge_metrics(&mut metrics, block_metrics);
                    pending.extend(block_pending);
                }
                ClassElement::MethodDefinition(_) | ClassElement::TSIndexSignature(_) => {}
            }
        }
        let class_scope = {
            let mut scope = self.scope.clone();
            scope.push(local_name.clone());
            scope
        };
        let has_work = metrics.statements > 0
            || metrics.expression_operations > 0
            || metrics.call_sites > 0
            || metrics.boolean_operators > 0
            || metrics.control_decisions > 0;
        if has_work {
            let initializer_name = format!("{}::<class>", class_scope.join("::"));
            self.add_report(
                initializer_name,
                FunctionKind::ClassInitializer,
                full,
                class.body.span,
                metrics,
                pending,
            );
        }

        let mut nested = DiscoveryWalker {
            source: self.source,
            token_index: self.token_index,
            line_map: self.line_map,
            category: self.category,
            scope: class_scope,
            depth: self.depth,
            in_function: false,
            functions: &mut *self.functions,
        };
        for element in &class.body.body {
            nested.visit_class_element(element);
        }
    }

    fn collect_class_property(
        &mut self,
        key: &PropertyKey<'_>,
        computed: bool,
        decorators: &[Decorator<'_>],
        value: Option<&Expression<'_>>,
        metrics: &mut Metrics,
        pending: &mut Vec<PendingContribution>,
    ) {
        if computed {
            self.visit_property_key(key);
            if let Some(expression) = key.as_expression() {
                add_expression_metrics(expression, metrics, pending, self.line_map, self.depth);
            }
        }
        for decorator in decorators {
            self.visit_expression(&decorator.expression);
        }
        if let Some(value) = value {
            metrics.statements += 1;
            measure_into(value, metrics, pending, self.line_map, self.depth);
        }
    }

    fn collect_method(&mut self, method: &MethodDefinition<'_>) {
        let Some(body) = method.value.body.as_deref() else {
            return;
        };
        let local_name = property_key_name(&method.key);
        let name = self.qualified_name(&local_name);
        let full = method.span;
        let parameters = Self::explicit_parameter_count(&method.value.params);
        let (mut metrics, mut pending) =
            metrics_for_statements(&body.statements, self.line_map, full, 0, parameters);
        Self::parameter_contributions(&method.value.params, &mut pending);
        metrics.parameters = parameters;
        metrics.explicit_parameters = parameters;
        self.add_report(
            name.clone(),
            FunctionKind::Method,
            full,
            body.span,
            metrics,
            pending,
        );
        let mut nested = DiscoveryWalker {
            source: self.source,
            token_index: self.token_index,
            line_map: self.line_map,
            category: self.category,
            scope: {
                let mut scope = self.scope.clone();
                scope.push(local_name);
                scope
            },
            depth: 0,
            in_function: true,
            functions: &mut *self.functions,
        };
        nested.visit_body(&body.statements);
    }
}

/// A module containing only callable declarations has no separate
/// initialization unit. The callables already own their bodies, and charging
/// their declarations again would make arrow-heavy React modules look more
/// complex than equivalent function-declaration modules.
fn module_statement_has_runtime_work(statement: &Statement<'_>) -> bool {
    match statement {
        Statement::FunctionDeclaration(_)
        | Statement::ClassDeclaration(_)
        | Statement::ImportDeclaration(_)
        | Statement::ExportNamedDeclaration(_)
        | Statement::ExportFromDeclaration(_)
        | Statement::ExportAllDeclaration(_)
        | Statement::TSTypeAliasDeclaration(_)
        | Statement::TSInterfaceDeclaration(_)
        | Statement::TSEnumDeclaration(_)
        | Statement::TSModuleDeclaration(_)
        | Statement::TSGlobalDeclaration(_)
        | Statement::TSImportEqualsDeclaration(_)
        | Statement::EmptyStatement(_) => false,
        Statement::VariableDeclaration(declaration) => {
            variable_declaration_has_runtime_work(declaration)
        }
        Statement::ExportDeclaration(export) => declaration_has_runtime_work(&export.declaration),
        Statement::ExportDefaultDeclaration(export) => !matches!(
            &export.declaration,
            ExportDefaultDeclarationKind::FunctionDeclaration(_)
                | ExportDefaultDeclarationKind::ClassDeclaration(_)
                | ExportDefaultDeclarationKind::TSInterfaceDeclaration(_)
        ),
        _ => true,
    }
}

fn declaration_has_runtime_work(declaration: &Declaration<'_>) -> bool {
    match declaration {
        Declaration::VariableDeclaration(declaration) => {
            variable_declaration_has_runtime_work(declaration)
        }
        Declaration::FunctionDeclaration(_)
        | Declaration::ClassDeclaration(_)
        | Declaration::TSTypeAliasDeclaration(_)
        | Declaration::TSInterfaceDeclaration(_)
        | Declaration::TSEnumDeclaration(_)
        | Declaration::TSModuleDeclaration(_)
        | Declaration::TSGlobalDeclaration(_)
        | Declaration::TSImportEqualsDeclaration(_) => false,
    }
}

fn variable_declaration_has_runtime_work(declaration: &VariableDeclaration<'_>) -> bool {
    declaration
        .declarations
        .iter()
        .filter_map(|declarator| declarator.init.as_ref())
        .any(|expression| !is_callable_initializer(expression))
}

fn is_callable_initializer(expression: &Expression<'_>) -> bool {
    match expression {
        Expression::ArrowFunctionExpression(_)
        | Expression::FunctionExpression(_)
        | Expression::ClassExpression(_) => true,
        Expression::ParenthesizedExpression(expression) => {
            is_callable_initializer(&expression.expression)
        }
        Expression::TSAsExpression(expression) => is_callable_initializer(&expression.expression),
        Expression::TSSatisfiesExpression(expression) => {
            is_callable_initializer(&expression.expression)
        }
        Expression::TSTypeAssertion(expression) => is_callable_initializer(&expression.expression),
        Expression::TSNonNullExpression(expression) => {
            is_callable_initializer(&expression.expression)
        }
        Expression::TSInstantiationExpression(expression) => {
            is_callable_initializer(&expression.expression)
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    fn analyze(source: &str, mode: SourceMode) -> FileAnalysis {
        analyze_source(source, mode, Category::Production).expect("source should parse")
    }

    #[test]
    fn source_modes_are_explicit_and_jsx_is_rejected_in_js() {
        assert_eq!(
            SourceMode::from_path(Path::new("component.tsx")),
            Ok(SourceMode::TypeScriptJsx)
        );
        assert!(SourceMode::from_path(Path::new("component.css")).is_err());
        assert!(
            analyze_file(
                Path::new("component.js"),
                "const x = <View />;",
                Category::Production
            )
            .is_err()
        );
        assert!(
            analyze_file(
                Path::new("component.jsx"),
                "const x = <View />;",
                Category::Production
            )
            .is_ok()
        );
    }

    #[test]
    fn typescript_type_space_is_erased_from_structural_metrics() {
        let typed = analyze(
            "interface Row { value: string }\ntype Maybe = Row | null;\nconst value: number = 1;\n",
            SourceMode::TypeScript,
        );
        let module = typed
            .functions
            .iter()
            .find(|function| function.kind == FunctionKind::ModuleInitializer)
            .expect("runtime variable should create a module initializer");
        assert_eq!(module.metrics.expression_operations, 0);
        assert_eq!(module.metrics.boolean_operators, 0);
        assert_eq!(module.score.units, 10);
    }

    #[test]
    fn jsx_markup_is_free_but_embedded_expressions_are_scored() {
        let file = analyze(
            "const App = () => <View title={ready ? 'yes' : 'no'}>{ready && <Child />}</View>;\n",
            SourceMode::JavaScriptJsx,
        );
        let app = file
            .functions
            .iter()
            .find(|function| function.name == "App")
            .expect("named arrow should be reported");
        assert_eq!(app.kind, FunctionKind::Closure);
        assert_eq!(app.metrics.control_decisions, 1);
        assert_eq!(app.metrics.boolean_operators, 1);
        assert!(app.score.units > 10);
    }

    #[test]
    fn callable_bodies_are_exclusive_and_callbacks_retain_lexical_depth() {
        let file = analyze(
            "function outer(flag) {\n  if (flag) {\n    const callback = () => { if (flag) return 1; return 0; };\n  }\n}\n",
            SourceMode::JavaScript,
        );
        let outer = file
            .functions
            .iter()
            .find(|function| function.name == "outer")
            .expect("outer function should be reported");
        let callback = file
            .functions
            .iter()
            .find(|function| function.name == "outer::callback")
            .expect("callback should be reported");
        assert_eq!(outer.metrics.control_decisions, 1);
        assert_eq!(callback.metrics.control_decisions, 1);
        assert_eq!(callback.metrics.nesting_penalty, 1);
        assert_eq!(outer.metrics.closures, 1);
    }

    #[test]
    fn class_initializers_and_methods_are_separate_units() {
        let file = analyze(
            "class Counter {\n  value = makeValue();\n  static { register(); }\n  increment(step) { if (step) return step + 1; return 0; }\n}\n",
            SourceMode::JavaScript,
        );
        let initializer = file
            .functions
            .iter()
            .find(|function| function.kind == FunctionKind::ClassInitializer)
            .expect("runtime class fields should create an initializer");
        let method = file
            .functions
            .iter()
            .find(|function| function.name == "Counter::increment")
            .expect("method should be reported");
        assert_eq!(initializer.metrics.call_sites, 2);
        assert_eq!(method.metrics.control_decisions, 1);
        assert_eq!(
            file.functions
                .iter()
                .filter(|function| function.kind == FunctionKind::Method)
                .count(),
            1
        );
    }

    #[test]
    fn fingerprints_ignore_comments_and_whitespace() {
        let left = analyze("function f(x) { return x + 1; }", SourceMode::JavaScript);
        let right = analyze(
            "// leading\nfunction f( x ) { /* body */ return x+1; }",
            SourceMode::JavaScript,
        );
        let left = left
            .functions
            .iter()
            .find(|function| function.name == "f")
            .expect("left function");
        let right = right
            .functions
            .iter()
            .find(|function| function.name == "f")
            .expect("right function");
        assert_eq!(left.declaration_fingerprint, right.declaration_fingerprint);
        assert_eq!(left.body_fingerprint, right.body_fingerprint);
        assert!(left.tokens > 0);
    }

    #[test]
    fn evidence_collects_direct_and_method_calls_without_descending_into_children() {
        let file = analyze(
            "function helper(value) { return value; }\nfunction run() { return helper(1) + api.send(2); }\n",
            SourceMode::JavaScript,
        );
        let run_calls = file
            .evidence
            .calls
            .iter()
            .filter(|call| call.caller.name == "run")
            .collect::<Vec<_>>();
        assert_eq!(run_calls.len(), 2);
        assert!(run_calls.iter().any(|call| call.name == "helper"));
        assert!(run_calls.iter().any(|call| call.name == "send"));
        assert_eq!(file.evidence.calls.len(), 2);
    }
}

fn property_key_name(key: &PropertyKey<'_>) -> String {
    match key {
        PropertyKey::StaticIdentifier(identifier) => identifier.name.to_string(),
        PropertyKey::PrivateIdentifier(identifier) => format!("#{}", identifier.name),
        _ => "<computed>".to_owned(),
    }
}

fn merge_metrics(target: &mut Metrics, source: Metrics) {
    target.statements += source.statements;
    target.expression_operations += source.expression_operations;
    target.decisions += source.decisions;
    target.control_decisions += source.control_decisions;
    target.nesting_penalty += source.nesting_penalty;
    target.branches += source.branches;
    target.boolean_operators += source.boolean_operators;
    target.match_arms += source.match_arms;
    target.loops += source.loops;
    target.returns += source.returns;
    target.mutations += source.mutations;
    target.call_sites += source.call_sites;
    target.closures += source.closures;
    target.max_depth = target.max_depth.max(source.max_depth);
}

fn measure_into(
    expression: &Expression<'_>,
    metrics: &mut Metrics,
    pending: &mut Vec<PendingContribution>,
    line_map: &LineMap,
    depth: usize,
) {
    let (value_metrics, value_pending) =
        metrics_for_expression(expression, line_map, expression.span(), depth, 0);
    merge_metrics(metrics, value_metrics);
    pending.extend(value_pending);
}

fn add_expression_metrics(
    expression: &Expression<'_>,
    metrics: &mut Metrics,
    pending: &mut Vec<PendingContribution>,
    line_map: &LineMap,
    depth: usize,
) {
    let (value_metrics, value_pending) =
        metrics_for_expression(expression, line_map, expression.span(), depth, 0);
    merge_metrics(metrics, value_metrics);
    pending.extend(value_pending);
}

impl<'ast> VisitJs<'ast> for DiscoveryWalker<'_> {
    fn visit_program(&mut self, program: &Program<'ast>) {
        self.collect_module(program);
        self.visit_body(&program.body);
    }

    fn visit_statement(&mut self, statement: &Statement<'ast>) {
        match statement {
            Statement::FunctionDeclaration(function) => {
                let kind = if self.in_function {
                    FunctionKind::NestedFunction
                } else {
                    FunctionKind::Function
                };
                self.collect_function(function, kind, None, 0);
            }
            Statement::ClassDeclaration(class) => self.collect_class(class, None),
            Statement::BlockStatement(block) => self.visit_body(&block.body),
            Statement::IfStatement(node) => {
                self.visit_expression(&node.test);
                self.with_depth(|walker| walker.visit_statement(&node.consequent));
                if let Some(alternate) = &node.alternate {
                    if matches!(alternate, Statement::IfStatement(_)) {
                        self.visit_statement(alternate);
                    } else {
                        self.with_depth(|walker| walker.visit_statement(alternate));
                    }
                }
            }
            Statement::DoWhileStatement(node) => {
                self.with_depth(|walker| walker.visit_statement(&node.body));
                self.visit_expression(&node.test);
            }
            Statement::WhileStatement(node) => {
                self.visit_expression(&node.test);
                self.with_depth(|walker| walker.visit_statement(&node.body));
            }
            Statement::ForStatement(node) => {
                if let Some(init) = &node.init {
                    self.visit_for_statement_init(init);
                }
                if let Some(test) = &node.test {
                    self.visit_expression(test);
                }
                if let Some(update) = &node.update {
                    self.visit_expression(update);
                }
                self.with_depth(|walker| walker.visit_statement(&node.body));
            }
            Statement::ForInStatement(node) => {
                self.visit_for_statement_left(&node.left);
                self.visit_expression(&node.right);
                self.with_depth(|walker| walker.visit_statement(&node.body));
            }
            Statement::ForOfStatement(node) => {
                self.visit_for_statement_left(&node.left);
                self.visit_expression(&node.right);
                self.with_depth(|walker| walker.visit_statement(&node.body));
            }
            Statement::SwitchStatement(node) => {
                self.visit_expression(&node.discriminant);
                for case in &node.cases {
                    self.with_depth(|walker| walker.visit_switch_case(case));
                }
            }
            Statement::TryStatement(node) => {
                self.with_depth(|walker| walker.visit_block_statement(&node.block));
                if let Some(handler) = &node.handler {
                    self.with_depth(|walker| walker.visit_block_statement(&handler.body));
                }
                if let Some(finalizer) = &node.finalizer {
                    self.with_depth(|walker| walker.visit_block_statement(finalizer));
                }
            }
            Statement::WithStatement(node) => {
                self.visit_expression(&node.object);
                self.with_depth(|walker| walker.visit_statement(&node.body));
            }
            Statement::ExpressionStatement(node) => self.visit_expression(&node.expression),
            Statement::ThrowStatement(node) => self.visit_expression(&node.argument),
            Statement::ReturnStatement(node) => {
                if let Some(argument) = &node.argument {
                    self.visit_expression(argument);
                }
            }
            Statement::LabeledStatement(node) => self.visit_statement(&node.body),
            Statement::VariableDeclaration(declaration) => {
                self.visit_variable_declaration(declaration)
            }
            Statement::ExportDeclaration(node) => self.visit_declaration(&node.declaration),
            Statement::ExportDefaultDeclaration(node) => {
                self.visit_export_default_declaration(node);
            }
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSModuleDeclaration(_)
            | Statement::TSGlobalDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_)
            | Statement::EmptyStatement(_)
            | Statement::BreakStatement(_)
            | Statement::ContinueStatement(_)
            | Statement::DebuggerStatement(_) => {}
            _ => walk_js::walk_statement(self, statement),
        }
    }

    fn visit_declaration(&mut self, declaration: &Declaration<'ast>) {
        match declaration {
            Declaration::FunctionDeclaration(function) => {
                let kind = if self.in_function {
                    FunctionKind::NestedFunction
                } else {
                    FunctionKind::Function
                };
                self.collect_function(function, kind, None, 0);
            }
            Declaration::ClassDeclaration(class) => self.collect_class(class, None),
            Declaration::VariableDeclaration(declaration) => {
                self.visit_variable_declaration(declaration)
            }
            Declaration::TSTypeAliasDeclaration(_)
            | Declaration::TSInterfaceDeclaration(_)
            | Declaration::TSEnumDeclaration(_)
            | Declaration::TSModuleDeclaration(_)
            | Declaration::TSGlobalDeclaration(_)
            | Declaration::TSImportEqualsDeclaration(_) => {}
        }
    }

    fn visit_variable_declaration(&mut self, declaration: &VariableDeclaration<'ast>) {
        for declarator in &declaration.declarations {
            let hint = binding_name(&declarator.id);
            if let Some(init) = &declarator.init {
                self.visit_expression_with_hint(init, hint.as_deref());
            }
        }
    }

    fn visit_expression(&mut self, expression: &Expression<'ast>) {
        match expression {
            Expression::ArrowFunctionExpression(arrow) => self.collect_arrow(arrow, None),
            Expression::FunctionExpression(function) => {
                self.collect_function(function, FunctionKind::Closure, None, self.depth)
            }
            Expression::ClassExpression(class) => self.collect_class(class, None),
            _ => walk_js::walk_expression(self, expression),
        }
    }

    fn visit_function(&mut self, function: &Function<'ast>, _flags: ScopeFlags) {
        self.collect_function(function, FunctionKind::Closure, None, self.depth);
    }

    fn visit_arrow_function_expression(&mut self, arrow: &ArrowFunctionExpression<'ast>) {
        self.collect_arrow(arrow, None);
    }

    fn visit_class(&mut self, class: &Class<'ast>) {
        self.collect_class(class, None);
    }

    fn visit_class_body(&mut self, body: &ClassBody<'ast>) {
        for element in &body.body {
            self.visit_class_element(element);
        }
    }

    fn visit_class_element(&mut self, element: &ClassElement<'ast>) {
        match element {
            ClassElement::MethodDefinition(method) => self.collect_method(method),
            ClassElement::PropertyDefinition(property) => self.visit_property_definition(property),
            ClassElement::AccessorProperty(property) => self.visit_accessor_property(property),
            ClassElement::StaticBlock(block) => self.visit_static_block(block),
            ClassElement::TSIndexSignature(_) => {}
        }
    }

    fn visit_method_definition(&mut self, method: &MethodDefinition<'ast>) {
        self.collect_method(method);
    }

    fn visit_property_definition(&mut self, property: &PropertyDefinition<'ast>) {
        if property.computed {
            self.visit_property_key(&property.key);
        }
        for decorator in &property.decorators {
            self.visit_expression(&decorator.expression);
        }
        if let Some(value) = &property.value {
            self.visit_expression_with_hint(value, Some(&property_key_name(&property.key)));
        }
    }

    fn visit_accessor_property(&mut self, property: &AccessorProperty<'ast>) {
        if property.computed {
            self.visit_property_key(&property.key);
        }
        for decorator in &property.decorators {
            self.visit_expression(&decorator.expression);
        }
        if let Some(value) = &property.value {
            self.visit_expression_with_hint(value, Some(&property_key_name(&property.key)));
        }
    }

    fn visit_static_block(&mut self, block: &StaticBlock<'ast>) {
        self.visit_body(&block.body);
    }

    fn visit_module_declaration(&mut self, declaration: &ModuleDeclaration<'ast>) {
        match declaration {
            ModuleDeclaration::ExportDeclaration(node) => self.visit_declaration(&node.declaration),
            ModuleDeclaration::ExportDefaultDeclaration(node) => {
                self.visit_export_default_declaration(node)
            }
            _ => walk_js::walk_module_declaration(self, declaration),
        }
    }

    fn visit_export_default_declaration(&mut self, declaration: &ExportDefaultDeclaration<'ast>) {
        match &declaration.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                let kind = if self.in_function {
                    FunctionKind::NestedFunction
                } else {
                    FunctionKind::Function
                };
                self.collect_function(function, kind, None, 0);
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                self.collect_class(class, None)
            }
            ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => {}
            _ => walk_js::walk_export_default_declaration_kind(self, &declaration.declaration),
        }
    }
}

fn binding_name(pattern: &BindingPattern<'_>) -> Option<String> {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => Some(identifier.name.to_string()),
        _ => None,
    }
}

fn statement_span(statement: &Statement<'_>) -> Span {
    statement.span()
}

fn extract_javascript_evidence(
    source: &str,
    token_index: &TokenIndex,
    line_map: &LineMap,
    program: &Program<'_>,
    functions: &[FunctionReport],
) -> crate::evidence::FrontendEvidence {
    let owners = functions
        .iter()
        .map(|function| crate::evidence::LocalCallable {
            name: function.name.clone(),
            kind: function.kind.clone(),
            category: function.category,
            location: function.location.clone(),
        })
        .collect::<Vec<_>>();
    let mut walker = EvidenceWalker {
        source,
        token_index,
        line_map,
        owners,
        current: None,
        scope: Vec::new(),
        next_sequence: 0,
        evidence: crate::evidence::FrontendEvidence::default(),
    };
    let full = Span::new(0, source.len() as u32);
    if let Some(owner) = walker.owner_for(full, FunctionKind::ModuleInitializer, "<module>") {
        walker.collect_body(&owner, &program.body);
    } else {
        walker.visit_body_without_collection(&program.body);
    }
    walker.evidence
}

struct EvidenceWalker<'a> {
    source: &'a str,
    token_index: &'a TokenIndex,
    line_map: &'a LineMap,
    owners: Vec<crate::evidence::LocalCallable>,
    current: Option<crate::evidence::LocalCallable>,
    scope: Vec<String>,
    next_sequence: usize,
    evidence: crate::evidence::FrontendEvidence,
}

impl EvidenceWalker<'_> {
    fn location(&self, span: Span) -> Location {
        Location {
            start: self.line_map.position(span.start),
            end: self.line_map.end_position(span.end),
        }
    }

    fn owner_for(
        &self,
        span: Span,
        kind: FunctionKind,
        leaf: &str,
    ) -> Option<crate::evidence::LocalCallable> {
        let location = self.location(span);
        self.owners
            .iter()
            .find(|owner| owner.kind == kind && owner.location == location)
            .cloned()
            .or_else(|| {
                let mut parts = self.scope.clone();
                if !leaf.is_empty() {
                    parts.push(leaf.to_owned());
                }
                let name = parts.join("::");
                self.owners
                    .iter()
                    .find(|owner| owner.kind == kind && owner.name == name)
                    .cloned()
            })
    }

    fn collect_body(&mut self, owner: &crate::evidence::LocalCallable, body: &[Statement<'_>]) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let mut ordinal = 0;
        for statement in body {
            if is_callable_statement(statement) {
                continue;
            }
            let span = statement_span(statement);
            self.evidence
                .statements
                .push(crate::evidence::FrontendStatement {
                    caller: owner.clone(),
                    sequence,
                    ordinal,
                    location: self.location(span),
                    tokens: self.token_index.text_in(self.source, span),
                    scored_signals: statement_signals(statement, self.line_map),
                });
            ordinal += 1;
        }
        let previous = self.current.clone();
        self.current = Some(owner.clone());
        self.visit_body_without_collection(body);
        self.current = previous;
    }

    fn collect_expression(
        &mut self,
        owner: &crate::evidence::LocalCallable,
        expression: &Expression<'_>,
    ) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let span = expression.span();
        self.evidence
            .statements
            .push(crate::evidence::FrontendStatement {
                caller: owner.clone(),
                sequence,
                ordinal: 0,
                location: self.location(span),
                tokens: self.token_index.text_in(self.source, span),
                scored_signals: expression_signals(expression),
            });
        let previous = self.current.clone();
        self.current = Some(owner.clone());
        self.visit_expression_without_collection(expression);
        self.current = previous;
    }

    fn visit_body_without_collection(&mut self, body: &[Statement<'_>]) {
        for statement in body {
            self.visit_statement_without_collection(statement);
        }
    }

    fn visit_statement_without_collection(&mut self, statement: &Statement<'_>) {
        match statement {
            Statement::FunctionDeclaration(function) => {
                let kind = if self.current.as_ref().is_some_and(|owner| {
                    owner.kind == FunctionKind::Function
                        || owner.kind == FunctionKind::NestedFunction
                }) {
                    FunctionKind::NestedFunction
                } else {
                    FunctionKind::Function
                };
                if let Some(owner) =
                    self.owner_for(function.span, kind.clone(), function_name(function))
                {
                    self.collect_function_body(&owner, function);
                }
            }
            Statement::ClassDeclaration(class) => self.collect_class_body(class),
            Statement::BlockStatement(block) => self.visit_body_without_collection(&block.body),
            Statement::IfStatement(node) => {
                self.visit_expression_without_collection(&node.test);
                self.visit_statement_without_collection(&node.consequent);
                if let Some(alternate) = &node.alternate {
                    self.visit_statement_without_collection(alternate);
                }
            }
            Statement::DoWhileStatement(node) => {
                self.visit_statement_without_collection(&node.body);
                self.visit_expression_without_collection(&node.test);
            }
            Statement::WhileStatement(node) => {
                self.visit_expression_without_collection(&node.test);
                self.visit_statement_without_collection(&node.body);
            }
            Statement::ForStatement(node) => {
                if let Some(init) = &node.init {
                    self.visit_for_statement_init(init);
                }
                if let Some(test) = &node.test {
                    self.visit_expression_without_collection(test);
                }
                if let Some(update) = &node.update {
                    self.visit_expression_without_collection(update);
                }
                self.visit_statement_without_collection(&node.body);
            }
            Statement::ForInStatement(node) => {
                self.visit_for_statement_left(&node.left);
                self.visit_expression_without_collection(&node.right);
                self.visit_statement_without_collection(&node.body);
            }
            Statement::ForOfStatement(node) => {
                self.visit_for_statement_left(&node.left);
                self.visit_expression_without_collection(&node.right);
                self.visit_statement_without_collection(&node.body);
            }
            Statement::SwitchStatement(node) => {
                self.visit_expression_without_collection(&node.discriminant);
                for case in &node.cases {
                    if let Some(test) = &case.test {
                        self.visit_expression_without_collection(test);
                    }
                    self.visit_body_without_collection(&case.consequent);
                }
            }
            Statement::TryStatement(node) => {
                self.visit_body_without_collection(&node.block.body);
                if let Some(handler) = &node.handler {
                    self.visit_body_without_collection(&handler.body.body);
                }
                if let Some(finalizer) = &node.finalizer {
                    self.visit_body_without_collection(&finalizer.body);
                }
            }
            Statement::WithStatement(node) => {
                self.visit_expression_without_collection(&node.object);
                self.visit_statement_without_collection(&node.body);
            }
            Statement::ExpressionStatement(node) => {
                self.visit_expression_without_collection(&node.expression)
            }
            Statement::ThrowStatement(node) => {
                self.visit_expression_without_collection(&node.argument)
            }
            Statement::ReturnStatement(node) => {
                if let Some(argument) = &node.argument {
                    self.visit_expression_without_collection(argument);
                }
            }
            Statement::LabeledStatement(node) => {
                self.visit_statement_without_collection(&node.body)
            }
            Statement::VariableDeclaration(declaration) => {
                self.visit_variable_declaration(declaration)
            }
            Statement::ExportDeclaration(node) => {
                self.visit_declaration_without_collection(&node.declaration)
            }
            Statement::ExportDefaultDeclaration(node) => self.visit_export_default(node),
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSModuleDeclaration(_)
            | Statement::TSGlobalDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_)
            | Statement::EmptyStatement(_)
            | Statement::BreakStatement(_)
            | Statement::ContinueStatement(_)
            | Statement::DebuggerStatement(_) => {}
            _ => walk_js::walk_statement(self, statement),
        }
    }

    fn visit_declaration_without_collection(&mut self, declaration: &Declaration<'_>) {
        match declaration {
            Declaration::FunctionDeclaration(function) => {
                let kind = if self.current.is_some() {
                    FunctionKind::NestedFunction
                } else {
                    FunctionKind::Function
                };
                if let Some(owner) = self.owner_for(function.span, kind, function_name(function)) {
                    self.collect_function_body(&owner, function);
                }
            }
            Declaration::ClassDeclaration(class) => self.collect_class_body(class),
            Declaration::VariableDeclaration(declaration) => {
                self.visit_variable_declaration(declaration)
            }
            Declaration::TSTypeAliasDeclaration(_)
            | Declaration::TSInterfaceDeclaration(_)
            | Declaration::TSEnumDeclaration(_)
            | Declaration::TSModuleDeclaration(_)
            | Declaration::TSGlobalDeclaration(_)
            | Declaration::TSImportEqualsDeclaration(_) => {}
        }
    }

    fn visit_variable_declaration(&mut self, declaration: &VariableDeclaration<'_>) {
        for declarator in &declaration.declarations {
            if let Some(init) = &declarator.init {
                self.visit_expression_without_collection(init);
            }
        }
    }

    fn visit_expression_without_collection(&mut self, expression: &Expression<'_>) {
        match expression {
            Expression::FunctionExpression(function) => {
                if let Some(owner) = self.owner_for(function.span, FunctionKind::Closure, "") {
                    self.collect_function_body(&owner, function);
                }
            }
            Expression::ArrowFunctionExpression(arrow) => {
                if let Some(owner) = self.owner_for(arrow.span, FunctionKind::Closure, "") {
                    self.collect_arrow_body(&owner, arrow);
                }
            }
            Expression::ClassExpression(class) => self.collect_class_body(class),
            Expression::CallExpression(_)
            | Expression::NewExpression(_)
            | Expression::TaggedTemplateExpression(_)
            | Expression::ImportExpression(_) => {
                walk_js::walk_expression(self, expression);
            }
            _ => walk_js::walk_expression(self, expression),
        }
    }

    fn collect_function_body(
        &mut self,
        owner: &crate::evidence::LocalCallable,
        function: &Function<'_>,
    ) {
        let previous_scope = self.scope.clone();
        self.scope.push(
            owner
                .name
                .rsplit("::")
                .next()
                .unwrap_or(&owner.name)
                .to_owned(),
        );
        if let Some(body) = &function.body {
            self.collect_body(owner, &body.statements);
        }
        self.scope = previous_scope;
    }

    fn collect_arrow_body(
        &mut self,
        owner: &crate::evidence::LocalCallable,
        arrow: &ArrowFunctionExpression<'_>,
    ) {
        let previous_scope = self.scope.clone();
        self.scope.push(
            owner
                .name
                .rsplit("::")
                .next()
                .unwrap_or(&owner.name)
                .to_owned(),
        );
        match &arrow.body {
            ArrowFunctionBody::FunctionBody(body) => self.collect_body(owner, &body.statements),
            _ => self.collect_expression(owner, arrow.body.to_expression()),
        }
        self.scope = previous_scope;
    }

    fn collect_class_body(&mut self, class: &Class<'_>) {
        for element in &class.body.body {
            match element {
                ClassElement::MethodDefinition(method) => {
                    let name = property_key_name(&method.key);
                    if let Some(owner) = self.owner_for(method.span, FunctionKind::Method, &name) {
                        self.collect_function_body(&owner, &method.value);
                    }
                }
                ClassElement::PropertyDefinition(property) => {
                    if let Some(value) = &property.value {
                        self.visit_expression_without_collection(value);
                    }
                }
                ClassElement::AccessorProperty(property) => {
                    if let Some(value) = &property.value {
                        self.visit_expression_without_collection(value);
                    }
                }
                ClassElement::StaticBlock(block) => self.visit_body_without_collection(&block.body),
                ClassElement::TSIndexSignature(_) => {}
            }
        }
    }

    fn visit_export_default(&mut self, declaration: &ExportDefaultDeclaration<'_>) {
        match &declaration.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                if let Some(owner) = self.owner_for(
                    function.span,
                    FunctionKind::Function,
                    function_name(function),
                ) {
                    self.collect_function_body(&owner, function);
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => self.collect_class_body(class),
            ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => {}
            _ => walk_js::walk_export_default_declaration_kind(self, &declaration.declaration),
        }
    }
}

impl<'ast> VisitJs<'ast> for EvidenceWalker<'_> {
    fn visit_function(&mut self, function: &Function<'ast>, _flags: ScopeFlags) {
        if let Some(owner) = self.owner_for(function.span, FunctionKind::Closure, "") {
            self.collect_function_body(&owner, function);
        }
    }

    fn visit_arrow_function_expression(&mut self, arrow: &ArrowFunctionExpression<'ast>) {
        if let Some(owner) = self.owner_for(arrow.span, FunctionKind::Closure, "") {
            self.collect_arrow_body(&owner, arrow);
        }
    }

    fn visit_class(&mut self, class: &Class<'ast>) {
        self.collect_class_body(class);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'ast>) {
        if let Some(owner) = &self.current {
            let (name, reason) = call_target(&call.callee);
            self.evidence.calls.push(crate::evidence::FrontendCall {
                caller: owner.clone(),
                name,
                reason,
                location: self.location(call.span),
            });
        }
        walk_js::walk_call_expression(self, call);
    }

    fn visit_new_expression(&mut self, expression: &NewExpression<'ast>) {
        if let Some(owner) = &self.current {
            self.evidence.calls.push(crate::evidence::FrontendCall {
                caller: owner.clone(),
                name: expression_name(&expression.callee),
                reason: crate::evidence::CallResolutionReason::DirectLocal,
                location: self.location(expression.span),
            });
        }
        walk_js::walk_new_expression(self, expression);
    }
}

fn function_name<'a>(function: &'a Function<'a>) -> &'a str {
    function.id.as_ref().map_or("", |id| id.name.as_str())
}

fn call_target(expression: &Expression<'_>) -> (String, crate::evidence::CallResolutionReason) {
    match expression {
        Expression::Identifier(identifier) => (
            identifier.name.to_string(),
            crate::evidence::CallResolutionReason::DirectLocal,
        ),
        Expression::StaticMemberExpression(member) => (
            member.property.name.to_string(),
            crate::evidence::CallResolutionReason::Method,
        ),
        Expression::ComputedMemberExpression(_) => (
            "<dynamic>".to_owned(),
            crate::evidence::CallResolutionReason::Dynamic,
        ),
        _ => (
            "<dynamic>".to_owned(),
            crate::evidence::CallResolutionReason::Dynamic,
        ),
    }
}

fn expression_name(expression: &Expression<'_>) -> String {
    match expression {
        Expression::Identifier(identifier) => identifier.name.to_string(),
        _ => "<dynamic>".to_owned(),
    }
}

fn is_callable_statement(statement: &Statement<'_>) -> bool {
    matches!(
        statement,
        Statement::FunctionDeclaration(_) | Statement::ClassDeclaration(_)
    )
}

fn statement_signals(statement: &Statement<'_>, line_map: &LineMap) -> usize {
    let (metrics, _) = metrics_for_statements(
        std::slice::from_ref(statement),
        line_map,
        statement_span(statement),
        0,
        0,
    );
    metrics.control_decisions
        + metrics.boolean_operators
        + metrics.expression_operations
        + metrics.call_sites
}

fn expression_signals(expression: &Expression<'_>) -> usize {
    let mut visitor = SignalCollector { signals: 0 };
    visitor.visit_expression(expression);
    visitor.signals
}

struct SignalCollector {
    signals: usize,
}

impl<'ast> VisitJs<'ast> for SignalCollector {
    fn visit_expression(&mut self, expression: &Expression<'ast>) {
        match expression {
            Expression::BinaryExpression(_)
            | Expression::AssignmentExpression(_)
            | Expression::UnaryExpression(_)
            | Expression::UpdateExpression(_)
            | Expression::ComputedMemberExpression(_) => self.signals += 1,
            Expression::LogicalExpression(_) | Expression::ConditionalExpression(_) => {
                self.signals += 1
            }
            Expression::CallExpression(_) | Expression::NewExpression(_) => self.signals += 1,
            _ => {}
        }
        walk_js::walk_expression(self, expression);
    }

    fn visit_function(&mut self, _function: &Function<'ast>, _flags: ScopeFlags) {}

    fn visit_arrow_function_expression(&mut self, _arrow: &ArrowFunctionExpression<'ast>) {}

    fn visit_class(&mut self, _class: &Class<'ast>) {}
}

impl DiscoveryWalker<'_> {
    fn visit_expression_with_hint(&mut self, expression: &Expression<'_>, hint: Option<&str>) {
        match expression {
            Expression::ArrowFunctionExpression(arrow) => self.collect_arrow(arrow, hint),
            Expression::FunctionExpression(function) => {
                self.collect_function(function, FunctionKind::Closure, hint, self.depth)
            }
            Expression::ClassExpression(class) => self.collect_class(class, hint),
            _ => self.visit_expression(expression),
        }
    }
}

impl PendingContribution {
    const fn new(component: ScoreComponent, units: usize, span: Span) -> Self {
        Self {
            component,
            units,
            span,
        }
    }
}
