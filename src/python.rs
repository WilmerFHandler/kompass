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

use crate::identity;
use crate::model::{
    Category, FileAnalysis, FunctionKind, FunctionReport, Language, LineCounts, Location,
    MacroOpacity, Metrics, Position, ScoreComponent, ScoreContribution,
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
    let token_index = TokenIndex::new(parsed.tokens());
    let mut collector = UnitCollector {
        source,
        token_index: &token_index,
        line_map: &line_map,
        category,
        scope: Vec::new(),
        functions: Vec::new(),
    };
    collector.collect_module(module);
    let functions = collector.functions;
    let evidence = extract_python_evidence(source, &token_index, &line_map, module, &functions);

    Ok(FileAnalysis {
        lines: line_map.counts.clone(),
        tokens: token_index.total(),
        functions,
        macro_opacity: MacroOpacity::default(),
        evidence,
    })
}

/// Convenience wrapper for callers that do not have a path.
pub fn analyze(source: &str, category: Category) -> Result<FileAnalysis, String> {
    analyze_file(Path::new("<python>"), source, category)
}

/// Lower source-only call and statement facts beside the Ruff AST collector.
/// Name resolution remains intentionally conservative: only a unique direct
/// local function name can become a resolved edge in the common evidence
/// builder.
fn extract_python_evidence(
    source: &str,
    token_index: &TokenIndex,
    line_map: &LineMap,
    module: &ModModule,
    functions: &[FunctionReport],
) -> crate::evidence::FrontendEvidence {
    let imports = python_imports(module);
    let mut owners_by_exact = std::collections::BTreeMap::new();
    let mut owners_by_name = std::collections::BTreeMap::<
        (String, FunctionKind),
        Vec<crate::evidence::LocalCallable>,
    >::new();
    for function in functions {
        let owner = crate::evidence::LocalCallable {
            name: function.name.clone(),
            kind: function.kind.clone(),
            category: function.category,
            location: function.location.clone(),
        };
        owners_by_exact.insert(
            (
                function.name.clone(),
                function.kind.clone(),
                function.location.clone(),
            ),
            owner.clone(),
        );
        owners_by_name
            .entry((function.name.clone(), function.kind.clone()))
            .or_default()
            .push(owner);
    }
    let mut collector = PythonEvidenceCollector {
        source,
        token_index,
        line_map,
        owners_by_exact,
        owners_by_name,
        imports,
        scopes: Vec::new(),
        function_depth: 0,
        class_depth: 0,
        next_sequence: 0,
        evidence: crate::evidence::FrontendEvidence::default(),
    };
    let module_owner = collector.owner(
        collector.qualified_name("<module>"),
        FunctionKind::ModuleInitializer,
        TextRange::new(TextSize::new(0), text_size(source.len())),
    );
    if let Some(owner) = module_owner {
        collector.collect_body(&owner, &module.body, std::collections::BTreeSet::new());
    }
    collector.visit_body(&module.body);
    collector.evidence
}

#[derive(Clone, Debug, Default)]
struct PythonImports {
    names: std::collections::BTreeSet<String>,
    aliases: std::collections::BTreeSet<String>,
}

fn python_imports(module: &ModModule) -> PythonImports {
    let mut collector = PythonImportCollector::default();
    collector.visit_body(&module.body);
    collector.imports
}

#[derive(Default)]
struct PythonImportCollector {
    imports: PythonImports,
}

impl<'ast> Visitor<'ast> for PythonImportCollector {
    fn visit_stmt(&mut self, statement: &'ast Stmt) {
        match statement {
            Stmt::Import(node) => {
                for alias in &node.names {
                    if let Some(asname) = &alias.asname {
                        self.imports.aliases.insert(asname.as_str().to_owned());
                    } else {
                        self.imports.names.insert(
                            alias
                                .name
                                .as_str()
                                .split('.')
                                .next()
                                .unwrap_or_default()
                                .to_owned(),
                        );
                    }
                }
            }
            Stmt::ImportFrom(node) => {
                for alias in &node.names {
                    if alias.name.as_str() == "*" {
                        continue;
                    }
                    if let Some(asname) = &alias.asname {
                        self.imports.aliases.insert(asname.as_str().to_owned());
                    } else {
                        self.imports.names.insert(alias.name.as_str().to_owned());
                    }
                }
            }
            _ => {}
        }
        visitor::walk_stmt(self, statement);
    }
}

struct PythonEvidenceCollector<'a> {
    source: &'a str,
    token_index: &'a TokenIndex,
    line_map: &'a LineMap,
    owners_by_exact: std::collections::BTreeMap<
        (String, FunctionKind, Location),
        crate::evidence::LocalCallable,
    >,
    owners_by_name:
        std::collections::BTreeMap<(String, FunctionKind), Vec<crate::evidence::LocalCallable>>,
    imports: PythonImports,
    scopes: Vec<String>,
    function_depth: usize,
    class_depth: usize,
    next_sequence: usize,
    evidence: crate::evidence::FrontendEvidence,
}

impl PythonEvidenceCollector<'_> {
    fn qualified_name(&self, leaf: &str) -> String {
        let mut parts = self.scopes.clone();
        parts.push(leaf.to_owned());
        if parts.is_empty() {
            leaf.to_owned()
        } else {
            parts.join("::")
        }
    }

    fn owner(
        &self,
        name: String,
        kind: FunctionKind,
        range: TextRange,
    ) -> Option<crate::evidence::LocalCallable> {
        let start = self.line_map.position(range.start());
        let end = self.line_map.end_position(range.end());
        let location = Location { start, end };
        self.owners_by_exact
            .get(&(name.clone(), kind.clone(), location))
            .cloned()
            .or_else(|| {
                self.owners_by_name
                    .get(&(name, kind))
                    .and_then(|owners| match owners.as_slice() {
                        [owner] => Some(owner.clone()),
                        _ => None,
                    })
            })
    }

    fn collect_body(
        &mut self,
        owner: &crate::evidence::LocalCallable,
        body: &[Stmt],
        parameter_names: std::collections::BTreeSet<String>,
    ) {
        let mut statements = PythonStatementCollector {
            owner: owner.clone(),
            source: self.source,
            token_index: self.token_index,
            line_map: self.line_map,
            next_sequence: &mut self.next_sequence,
            statements: &mut self.evidence.statements,
        };
        statements.collect_body(body);
        let bindings = python_bindings(body);
        let mut calls = PythonCallCollector {
            owner: owner.clone(),
            line_map: self.line_map,
            parameter_names,
            bindings,
            imports: &self.imports,
            calls: &mut self.evidence.calls,
        };
        calls.visit_body(body);
    }

    fn collect_lambda(&mut self, node: &ExprLambda) {
        let start = self.line_map.position(node.range.start());
        let label = format!("<lambda@{}:{}>", start.line, start.column);
        let name = self.qualified_name(&label);
        let Some(owner) = self.owner(name, FunctionKind::Lambda, node.range) else {
            return;
        };
        let parameter_names = node
            .parameters
            .as_deref()
            .map(|parameters| {
                parameters
                    .iter()
                    .map(|parameter| parameter.name().to_string())
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_default();
        let mut calls = PythonCallCollector {
            owner: owner.clone(),
            line_map: self.line_map,
            parameter_names,
            bindings: PythonBindings::default(),
            imports: &self.imports,
            calls: &mut self.evidence.calls,
        };
        calls.visit_expr(&node.body);
        let mut statements = PythonStatementCollector {
            owner: owner.clone(),
            source: self.source,
            token_index: self.token_index,
            line_map: self.line_map,
            next_sequence: &mut self.next_sequence,
            statements: &mut self.evidence.statements,
        };
        statements.collect_expression(&node.body);
    }
}

impl<'ast> Visitor<'ast> for PythonEvidenceCollector<'_> {
    fn visit_stmt(&mut self, statement: &'ast Stmt) {
        match statement {
            Stmt::FunctionDef(node) => {
                let kind = if self.function_depth > 0 {
                    FunctionKind::NestedFunction
                } else if self.class_depth > 0 {
                    FunctionKind::Method
                } else {
                    FunctionKind::Function
                };
                let name = self.qualified_name(node.name.as_str());
                if let Some(owner) = self.owner(name, kind, node.range) {
                    let parameters = node
                        .parameters
                        .iter()
                        .map(|parameter| parameter.name().to_string())
                        .collect();
                    self.collect_body(&owner, &node.body, parameters);
                }
                self.scopes.push(node.name.as_str().to_owned());
                self.function_depth += 1;
                for statement in &node.body {
                    self.visit_stmt(statement);
                }
                self.function_depth -= 1;
                self.scopes.pop();
            }
            Stmt::ClassDef(node) => {
                // UnitCollector names a class initializer after the class
                // scope (`Class::<class>`), so enter that scope before
                // looking up the owner. This keeps direct class-body work in
                // its exclusive initializer unit instead of dropping it.
                self.scopes.push(node.name.as_str().to_owned());
                let name = self.qualified_name("<class>");
                if let Some(owner) = self.owner(name, FunctionKind::ClassInitializer, node.range) {
                    self.collect_body(&owner, &node.body, std::collections::BTreeSet::new());
                }
                self.class_depth += 1;
                for statement in &node.body {
                    self.visit_stmt(statement);
                }
                self.class_depth -= 1;
                self.scopes.pop();
            }
            _ => visitor::walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'ast Expr) {
        if let Expr::Lambda(node) = expression {
            self.collect_lambda(node);
            if let Some(parameters) = &node.parameters {
                for parameter in parameters {
                    if let Some(default) = parameter.default() {
                        self.visit_expr(default);
                    }
                }
            }
            self.scopes.push(format!(
                "<lambda@{}:{}>",
                self.line_map.position(node.range.start()).line,
                self.line_map.position(node.range.start()).column
            ));
            self.visit_expr(&node.body);
            self.scopes.pop();
        } else {
            visitor::walk_expr(self, expression);
        }
    }
}

#[derive(Clone, Debug, Default)]
struct PythonBindings {
    assignments: std::collections::BTreeSet<String>,
    aliases: std::collections::BTreeSet<String>,
}

fn python_bindings(body: &[Stmt]) -> PythonBindings {
    let mut collector = PythonBindingCollector::default();
    collector.visit_body(body);
    collector.bindings
}

#[derive(Default)]
struct PythonBindingCollector {
    bindings: PythonBindings,
}

impl<'ast> Visitor<'ast> for PythonBindingCollector {
    fn visit_stmt(&mut self, statement: &'ast Stmt) {
        match statement {
            Stmt::Assign(node) => {
                let names = node
                    .targets
                    .iter()
                    .flat_map(python_target_names)
                    .collect::<Vec<_>>();
                if matches!(node.value.as_ref(), Expr::Name(_)) {
                    self.bindings.aliases.extend(names);
                } else {
                    self.bindings.assignments.extend(names);
                }
            }
            Stmt::AnnAssign(node) => {
                self.bindings
                    .assignments
                    .extend(python_target_names(&node.target));
            }
            Stmt::AugAssign(node) => {
                self.bindings
                    .assignments
                    .extend(python_target_names(&node.target));
            }
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => return,
            _ => {}
        }
        visitor::walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'ast Expr) {
        if let Expr::Named(node) = expression {
            self.bindings
                .assignments
                .extend(python_target_names(&node.target));
            visitor::walk_expr(self, expression);
        } else if !matches!(expression, Expr::Lambda(_)) {
            visitor::walk_expr(self, expression);
        }
    }
}

fn python_target_names(expression: &Expr) -> Vec<String> {
    match expression {
        Expr::Name(name) => vec![name.id.as_str().to_owned()],
        Expr::Tuple(tuple) => tuple.elts.iter().flat_map(python_target_names).collect(),
        Expr::List(list) => list.elts.iter().flat_map(python_target_names).collect(),
        _ => Vec::new(),
    }
}

struct PythonCallCollector<'a> {
    owner: crate::evidence::LocalCallable,
    line_map: &'a LineMap,
    parameter_names: std::collections::BTreeSet<String>,
    bindings: PythonBindings,
    imports: &'a PythonImports,
    calls: &'a mut Vec<crate::evidence::FrontendCall>,
}

impl PythonCallCollector<'_> {
    fn add(
        &mut self,
        name: String,
        reason: crate::evidence::CallResolutionReason,
        range: TextRange,
    ) {
        self.calls.push(crate::evidence::FrontendCall {
            caller: self.owner.clone(),
            name,
            reason,
            location: Location {
                start: self.line_map.position(range.start()),
                end: self.line_map.end_position(range.end()),
            },
        });
    }

    fn direct_reason(&self, name: &str) -> crate::evidence::CallResolutionReason {
        if self.parameter_names.contains(name) {
            crate::evidence::CallResolutionReason::Parameter
        } else if self.bindings.aliases.contains(name) {
            crate::evidence::CallResolutionReason::Alias
        } else if self.bindings.assignments.contains(name) {
            crate::evidence::CallResolutionReason::Assignment
        } else if self.imports.aliases.contains(name) {
            crate::evidence::CallResolutionReason::Alias
        } else if self.imports.names.contains(name) {
            crate::evidence::CallResolutionReason::Import
        } else {
            crate::evidence::CallResolutionReason::DirectLocal
        }
    }
}

impl<'ast> Visitor<'ast> for PythonCallCollector<'_> {
    fn visit_stmt(&mut self, statement: &'ast Stmt) {
        if matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
            return;
        }
        visitor::walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'ast Expr) {
        match expression {
            Expr::Call(node) => {
                let (name, reason) = match node.func.as_ref() {
                    Expr::Name(name) => {
                        let name = name.id.as_str().to_owned();
                        let reason = self.direct_reason(&name);
                        (name, reason)
                    }
                    Expr::Attribute(attribute) => (
                        attribute.attr.as_str().to_owned(),
                        crate::evidence::CallResolutionReason::Method,
                    ),
                    _ => (
                        "<dynamic>".to_owned(),
                        crate::evidence::CallResolutionReason::Dynamic,
                    ),
                };
                self.add(name, reason, node.range());
                visitor::walk_expr(self, expression);
            }
            Expr::Lambda(_) => {}
            _ => visitor::walk_expr(self, expression),
        }
    }
}

struct PythonStatementCollector<'a> {
    owner: crate::evidence::LocalCallable,
    source: &'a str,
    token_index: &'a TokenIndex,
    line_map: &'a LineMap,
    next_sequence: &'a mut usize,
    statements: &'a mut Vec<crate::evidence::FrontendStatement>,
}

impl PythonStatementCollector<'_> {
    fn collect_body(&mut self, body: &[Stmt]) {
        let sequence = *self.next_sequence;
        *self.next_sequence = self.next_sequence.saturating_add(1);
        let mut ordinal = 0;
        for statement in body {
            if matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
                continue;
            }
            let range = statement.range();
            self.statements.push(crate::evidence::FrontendStatement {
                caller: self.owner.clone(),
                sequence,
                ordinal,
                location: self.location(range),
                tokens: self.token_index.text_in(self.source, range),
                scored_signals: python_statement_signals(statement),
            });
            ordinal += 1;
        }
        for statement in body {
            if !matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
                self.visit_stmt(statement);
            }
        }
    }

    fn collect_expression(&mut self, expression: &Expr) {
        let sequence = *self.next_sequence;
        *self.next_sequence = self.next_sequence.saturating_add(1);
        self.statements.push(crate::evidence::FrontendStatement {
            caller: self.owner.clone(),
            sequence,
            ordinal: 0,
            location: self.location(expression.range()),
            tokens: self.token_index.text_in(self.source, expression.range()),
            scored_signals: python_expression_signals(expression),
        });
    }

    fn location(&self, range: TextRange) -> Location {
        Location {
            start: self.line_map.position(range.start()),
            end: self.line_map.end_position(range.end()),
        }
    }
}

impl<'ast> Visitor<'ast> for PythonStatementCollector<'_> {
    fn visit_body(&mut self, body: &'ast [Stmt]) {
        self.collect_body(body);
    }

    fn visit_stmt(&mut self, statement: &'ast Stmt) {
        if matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
            return;
        }
        visitor::walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'ast Expr) {
        if !matches!(expression, Expr::Lambda(_)) {
            visitor::walk_expr(self, expression);
        }
    }
}

#[derive(Default)]
struct PythonSignalCollector {
    signals: usize,
}

impl<'ast> Visitor<'ast> for PythonSignalCollector {
    fn visit_stmt(&mut self, statement: &'ast Stmt) {
        match statement {
            Stmt::If(_) | Stmt::For(_) | Stmt::While(_) | Stmt::Match(_) | Stmt::Try(_) => {
                self.signals += 1
            }
            Stmt::Assign(_) | Stmt::AugAssign(_) | Stmt::AnnAssign(_) | Stmt::TypeAlias(_) => {
                self.signals += 1
            }
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => return,
            _ => {}
        }
        visitor::walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'ast Expr) {
        match expression {
            Expr::BoolOp(node) => {
                self.signals += node.values.len().saturating_sub(1);
            }
            Expr::BinOp(_)
            | Expr::UnaryOp(_)
            | Expr::Compare(_)
            | Expr::Call(_)
            | Expr::Subscript(_)
            | Expr::Await(_)
            | Expr::Yield(_)
            | Expr::YieldFrom(_)
            | Expr::If(_) => self.signals += 1,
            Expr::Lambda(_) => return,
            Expr::ListComp(node) => self.signals += node.generators.len(),
            Expr::SetComp(node) => self.signals += node.generators.len(),
            Expr::DictComp(node) => self.signals += node.generators.len(),
            Expr::Generator(node) => self.signals += node.generators.len(),
            _ => {}
        }
        visitor::walk_expr(self, expression);
    }
}

fn python_statement_signals(statement: &Stmt) -> usize {
    let mut collector = PythonSignalCollector::default();
    collector.visit_stmt(statement);
    collector.signals
}

fn python_expression_signals(expression: &Expr) -> usize {
    let mut collector = PythonSignalCollector::default();
    collector.visit_expr(expression);
    collector.signals
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

#[derive(Clone, Debug, Default)]
struct TokenIndex {
    starts: Vec<usize>,
    ends: Vec<usize>,
    prefix: Vec<usize>,
}

impl TokenIndex {
    fn new(tokens: &Tokens) -> Self {
        let mut ranges = tokens
            .iter()
            .filter(|token| is_counted_token(token.kind()))
            .map(|token| {
                let range = token.range();
                (range.start().to_usize(), range.end().to_usize())
            })
            .collect::<Vec<_>>();
        ranges.sort_unstable();

        let mut starts = Vec::with_capacity(ranges.len());
        let mut ends = Vec::with_capacity(ranges.len());
        let mut prefix = Vec::with_capacity(ranges.len() + 1);
        prefix.push(0);
        for (start, end) in ranges {
            starts.push(start);
            ends.push(end);
            let previous = *prefix.last().unwrap_or(&0);
            prefix.push(previous + 1);
        }
        Self {
            starts,
            ends,
            prefix,
        }
    }

    fn total(&self) -> usize {
        self.prefix.last().copied().unwrap_or(0)
    }

    /// Count tokens fully contained in a range using two binary searches and
    /// the prefix count. The construction is linear in the file token count;
    /// each callable query is logarithmic instead of scanning the file.
    fn count_in(&self, range: TextRange) -> usize {
        let start = range.start().to_usize();
        let end = range.end().to_usize();
        let first = self.starts.partition_point(|candidate| *candidate < start);
        let last = self.ends.partition_point(|candidate| *candidate <= end);
        self.prefix
            .get(last)
            .copied()
            .unwrap_or(0)
            .saturating_sub(self.prefix.get(first).copied().unwrap_or(0))
    }

    fn text_in(&self, source: &str, range: TextRange) -> Vec<String> {
        let start = range.start().to_usize();
        let end = range.end().to_usize();
        let first = self.starts.partition_point(|candidate| *candidate < start);
        let last = self.starts.partition_point(|candidate| *candidate < end);
        (first..last)
            .filter(|index| self.ends[*index] <= end)
            .filter_map(|index| source.get(self.starts[index]..self.ends[index]))
            .map(str::to_owned)
            .collect()
    }
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
    token_index: &'a TokenIndex,
    line_map: &'a LineMap,
    category: Category,
    scope: Vec<String>,
    functions: Vec<FunctionReport>,
}

#[derive(Clone, Copy)]
struct UnitRanges {
    full: TextRange,
    declaration: TextRange,
    body: TextRange,
}

impl UnitRanges {
    fn new(full: TextRange, body: TextRange) -> Self {
        Self {
            full,
            declaration: declaration_range(full, body),
            body,
        }
    }

    fn initializer(full: TextRange, body: TextRange) -> Self {
        Self {
            full,
            declaration: full,
            body,
        }
    }
}

impl<'a> UnitCollector<'a> {
    fn collect_module(&mut self, module: &ModModule) {
        let range = TextRange::new(TextSize::new(0), text_size(self.source.len()));
        let (metrics, contributions) = measure_body_with_contributions(
            &module.body,
            self.line_map.code_lines_in(range),
            0,
            0,
            0,
        );
        if initializer_has_work(&module.body, &metrics) {
            self.functions.push(self.make_report(
                self.qualified_name("<module>"),
                FunctionKind::ModuleInitializer,
                UnitRanges::initializer(range, body_range(&module.body).unwrap_or(range)),
                metrics,
                contributions,
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
        let (metrics, contributions) = measure_body_with_contributions(
            &node.body,
            self.line_map.code_lines_in(range),
            node.parameters.len(),
            explicit_parameters,
            depth,
        );
        let is_method = kind == FunctionKind::Method;
        self.scope.push(name.clone());
        let qualified_name = self.qualified_name("");
        self.scope.pop();
        let body = body_range(&node.body).unwrap_or(range);
        self.functions.push(self.make_report(
            qualified_name,
            kind,
            UnitRanges::new(range, body),
            metrics,
            with_parameter_contributions(contributions, &node.parameters, is_method, node),
        ));

        self.collect_function_header(node, depth);
        self.scope.push(name);
        self.collect_body(&node.body, depth, ScopeKind::Function);
        self.scope.pop();
    }

    fn add_class(&mut self, node: &StmtClassDef, depth: usize) {
        let name = node.name.to_string();
        let range = node.range;
        let (metrics, contributions) = measure_body_with_contributions(
            &node.body,
            self.line_map.code_lines_in(range),
            0,
            0,
            depth,
        );
        self.scope.push(name.clone());
        if initializer_has_work(&node.body, &metrics) {
            let qualified_name = self.qualified_name("<class>");
            self.functions.push(self.make_report(
                qualified_name,
                FunctionKind::ClassInitializer,
                UnitRanges::initializer(range, body_range(&node.body).unwrap_or(range)),
                metrics,
                contributions,
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
        let (metrics, contributions) = measure_expression_with_contributions(
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
        let body = node.body.range();
        self.functions.push(self.make_report(
            qualified_name,
            FunctionKind::Lambda,
            UnitRanges::new(range, body),
            metrics,
            with_lambda_parameter_contributions(contributions, parameters),
        ));

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
        ranges: UnitRanges,
        metrics: Metrics,
        mut contributions: Vec<PendingContribution>,
    ) -> FunctionReport {
        let UnitRanges {
            full: range,
            declaration,
            body,
        } = ranges;
        let start = self.line_map.position(range.start());
        let end = self.line_map.end_position(range.end());
        let lines = end.line.saturating_sub(start.line) + 1;
        contributions.push(PendingContribution {
            component: ScoreComponent::Boundary,
            units: 10,
            range,
        });
        let contributions = contributions
            .into_iter()
            .map(|contribution| ScoreContribution {
                component: contribution.component,
                units: contribution.units,
                location: Location {
                    start: self.line_map.position(contribution.range.start()),
                    end: self.line_map.end_position(contribution.range.end()),
                },
            })
            .collect::<Vec<_>>();
        let location = Location {
            start: start.clone(),
            end: end.clone(),
        };
        FunctionReport {
            snapshot_id: String::new(),
            declaration_fingerprint: identity::range_fingerprint(
                self.source,
                declaration.start().to_usize(),
                declaration.end().to_usize(),
                Language::Python,
            ),
            body_fingerprint: identity::range_fingerprint(
                self.source,
                body.start().to_usize(),
                body.end().to_usize(),
                Language::Python,
            ),
            name,
            kind,
            category: self.category,
            location: Location {
                start: start.clone(),
                end: end.clone(),
            },
            lines,
            tokens: self.token_index.count_in(range),
            score: score::score_with_contributions(&metrics, location, contributions),
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

fn body_range(body: &[Stmt]) -> Option<TextRange> {
    let start = body.first()?.range().start();
    let end = body.last()?.range().end();
    Some(TextRange::new(start, end))
}

fn declaration_range(range: TextRange, body: TextRange) -> TextRange {
    TextRange::new(range.start(), body.start().max(range.start()))
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

fn with_parameter_contributions(
    mut contributions: Vec<PendingContribution>,
    parameters: &Parameters,
    is_method: bool,
    function: &StmtFunctionDef,
) -> Vec<PendingContribution> {
    let skip_receiver = is_method && !is_staticmethod(function);
    for (index, parameter) in parameters.iter().enumerate() {
        if skip_receiver && index == 0 && matches!(parameter.name().as_ref(), "self" | "cls") {
            continue;
        }
        contributions.push(PendingContribution {
            component: ScoreComponent::ExplicitParameters,
            units: 2,
            range: parameter.range(),
        });
    }
    contributions
}

fn with_lambda_parameter_contributions(
    mut contributions: Vec<PendingContribution>,
    parameters: Option<&Parameters>,
) -> Vec<PendingContribution> {
    if let Some(parameters) = parameters {
        for parameter in parameters {
            contributions.push(PendingContribution {
                component: ScoreComponent::ExplicitParameters,
                units: 2,
                range: parameter.range(),
            });
        }
    }
    contributions
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

#[derive(Clone, Copy, Debug)]
struct PendingContribution {
    component: ScoreComponent,
    units: usize,
    range: TextRange,
}

fn measure_body_with_contributions(
    body: &[Stmt],
    code_lines: usize,
    parameters: usize,
    explicit_parameters: usize,
    base_depth: usize,
) -> (Metrics, Vec<PendingContribution>) {
    let mut visitor = MetricsCollector {
        metrics: Metrics {
            code_lines,
            parameters,
            explicit_parameters,
            ..Metrics::default()
        },
        depth: base_depth,
        contributions: Vec::new(),
    };
    visitor.visit_body(body);
    (visitor.metrics, visitor.contributions)
}

fn measure_expression_with_contributions(
    expression: &Expr,
    code_lines: usize,
    parameters: usize,
    explicit_parameters: usize,
    base_depth: usize,
) -> (Metrics, Vec<PendingContribution>) {
    let mut visitor = MetricsCollector {
        metrics: Metrics {
            code_lines,
            parameters,
            explicit_parameters,
            ..Metrics::default()
        },
        depth: base_depth,
        contributions: Vec::new(),
    };
    visitor.visit_expr(expression);
    visitor.metrics.statements = 1;
    (visitor.metrics, visitor.contributions)
}

struct MetricsCollector {
    metrics: Metrics,
    depth: usize,
    contributions: Vec<PendingContribution>,
}

impl MetricsCollector {
    fn push(&mut self, component: ScoreComponent, units: usize, range: TextRange) {
        self.contributions.push(PendingContribution {
            component,
            units,
            range,
        });
    }

    fn branch(&mut self, range: TextRange) {
        self.metrics.branches += 1;
        self.metrics.decisions += 1;
        self.metrics.control_decisions += 1;
        self.metrics.nesting_penalty += self.depth;
        self.metrics.max_depth = self.metrics.max_depth.max(self.depth + 1);
        self.push(ScoreComponent::ControlDecisions, 10, range);
        if self.depth > 0 {
            self.push(
                ScoreComponent::NestingPenalty,
                self.depth.saturating_mul(10),
                range,
            );
        }
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
            self.branch(generator.range);
            self.metrics.loops += 1;
            self.visit_expr(&generator.iter);
            self.visit_expr(&generator.target);
            self.depth += 1;
            for condition in &generator.ifs {
                self.branch(condition.range());
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
                self.branch(node.range);
                self.visit_expr(&node.test);
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                for clause in &node.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.branch(clause.range);
                        self.visit_expr(test);
                        self.with_depth(|visitor| visitor.visit_body(&clause.body));
                    } else {
                        self.with_depth(|visitor| visitor.visit_body(&clause.body));
                    }
                }
            }
            Stmt::For(node) => {
                self.branch(node.range);
                self.metrics.loops += 1;
                self.visit_expr(&node.iter);
                self.visit_expr(&node.target);
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                self.with_depth(|visitor| visitor.visit_body(&node.orelse));
            }
            Stmt::While(node) => {
                self.branch(node.range);
                self.metrics.loops += 1;
                self.visit_expr(&node.test);
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                self.with_depth(|visitor| visitor.visit_body(&node.orelse));
            }
            Stmt::With(node) => {
                self.metrics.expression_operations += node.items.len();
                for item in &node.items {
                    self.push(ScoreComponent::ExpressionOperations, 1, item.range);
                }
                for item in &node.items {
                    self.visit_with_item(item);
                }
                self.visit_body(&node.body);
            }
            Stmt::Match(node) => {
                self.branch(node.range);
                self.metrics.match_arms += node.cases.len();
                for case in &node.cases {
                    self.push(ScoreComponent::MatchArms, 2, case.range);
                }
                self.visit_expr(&node.subject);
                for case in &node.cases {
                    self.with_depth(|visitor| {
                        visitor.visit_pattern(&case.pattern);
                        if let Some(guard) = &case.guard {
                            visitor.branch(guard.range());
                            visitor.visit_expr(guard);
                        }
                        visitor.visit_body(&case.body);
                    });
                }
            }
            Stmt::Try(node) => {
                self.with_depth(|visitor| visitor.visit_body(&node.body));
                for handler in &node.handlers {
                    self.branch(handler.range());
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
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                self.visit_expr(&node.value);
                for target in &node.targets {
                    self.visit_expr(target);
                }
            }
            Stmt::AugAssign(node) => {
                self.metrics.mutations += 1;
                self.metrics.expression_operations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                self.visit_expr(&node.value);
                self.visit_expr(&node.target);
            }
            Stmt::AnnAssign(node) => {
                self.metrics.mutations += 1;
                self.metrics.expression_operations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                self.visit_expr(&node.annotation);
                self.visit_expr(&node.target);
                if let Some(value) = &node.value {
                    self.visit_expr(value);
                }
            }
            Stmt::TypeAlias(node) => {
                self.metrics.mutations += 1;
                self.metrics.expression_operations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
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
                for _ in 0..links {
                    self.push(ScoreComponent::BooleanOperators, 5, expr.range());
                }
                for value in values {
                    self.visit_expr(value);
                }
            }
            Expr::BinOp(node) => {
                self.metrics.expression_operations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                self.visit_expr(&node.left);
                self.visit_expr(&node.right);
            }
            Expr::UnaryOp(node) => {
                self.metrics.expression_operations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                self.visit_expr(&node.operand);
            }
            Expr::Compare(ExprCompare {
                ops,
                left,
                comparators,
                ..
            }) => {
                self.metrics.expression_operations += ops.len();
                for _ in ops.iter() {
                    self.push(ScoreComponent::ExpressionOperations, 1, expr.range());
                }
                self.visit_expr(left);
                for comparator in comparators {
                    self.visit_expr(comparator);
                }
            }
            Expr::Call(ExprCall {
                func, arguments, ..
            }) => {
                self.metrics.call_sites += 1;
                self.push(ScoreComponent::CallSites, 2, expr.range());
                self.visit_expr(func);
                self.visit_arguments(arguments);
            }
            Expr::Subscript(node) => {
                self.metrics.expression_operations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                self.visit_expr(&node.value);
                self.visit_expr(&node.slice);
            }
            Expr::Await(node) => {
                self.metrics.expression_operations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                self.visit_expr(&node.value);
            }
            Expr::Yield(node) => {
                self.metrics.expression_operations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                if let Some(value) = &node.value {
                    self.visit_expr(value);
                }
            }
            Expr::YieldFrom(node) => {
                self.metrics.expression_operations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                self.visit_expr(&node.value);
            }
            Expr::Named(node) => {
                self.metrics.expression_operations += 1;
                self.metrics.mutations += 1;
                self.push(ScoreComponent::ExpressionOperations, 1, node.range);
                self.visit_expr(&node.value);
                self.visit_expr(&node.target);
            }
            Expr::If(node) => {
                self.branch(node.range);
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

    #[test]
    fn token_index_matches_the_reference_range_scan() {
        let source = "def f(value):\n    return value + 1  # comment\n";
        let options =
            ParseOptions::from(PySourceType::Python).with_target_version(PythonVersion::PY314);
        let parsed = parse_unchecked(source, options);
        let index = TokenIndex::new(parsed.tokens());
        for start in (0..=source.len()).step_by(3) {
            for end in (start..=source.len()).step_by(5) {
                let range = TextRange::new(text_size(start), text_size(end));
                let reference = parsed
                    .tokens()
                    .iter()
                    .filter(|token| {
                        let token_range = token.range();
                        token_range.start() >= range.start()
                            && token_range.end() <= range.end()
                            && is_counted_token(token.kind())
                    })
                    .count();
                assert_eq!(index.count_in(range), reference, "range {start}..{end}");
            }
        }
    }

    #[test]
    fn token_index_shape_is_linear_and_queries_use_prefix_counts() {
        let source = "x + 1\n".repeat(256);
        let options =
            ParseOptions::from(PySourceType::Python).with_target_version(PythonVersion::PY314);
        let parsed = parse_unchecked(&source, options);
        let index = TokenIndex::new(parsed.tokens());
        assert_eq!(index.prefix.len(), index.starts.len() + 1);
        assert_eq!(index.total(), index.starts.len());
        let full = TextRange::new(TextSize::new(0), text_size(source.len()));
        assert_eq!(index.count_in(full), index.total());
    }

    #[test]
    fn score_contributions_reconcile_for_python_units() {
        let file = analyze_source(
            "def f(value):\n    if value and value > 1:\n        return call(value)\n",
        );
        for function in file.functions {
            assert_eq!(
                function.score.units,
                function
                    .score
                    .contributions
                    .iter()
                    .map(|contribution| contribution.units)
                    .sum::<usize>()
            );
        }
    }
}
