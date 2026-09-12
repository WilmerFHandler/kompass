use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, FnArg, ItemFn, ItemMod, Meta, Token};

use crate::discover::{DiscoveredFile, Discovery};
use crate::model::{
    AnalysisError, Burden, Category, CategorySummary, Coverage, ErrorKind, FileReport,
    FunctionKind, FunctionReport, LineCounts, Location, MacroOpacity, Position, Report,
    ScoringModel, Summary,
};
use crate::score;
use crate::tokens::{self, LexedSource, TokenPosition};

#[derive(Clone, Copy, Debug, Default)]
pub struct AnalysisOptions {
    pub model: ScoringModel,
}

/// Analyze each discovered source file independently. A file that cannot be
/// read, lexed, or parsed is recorded as an error while successful files still
/// produce a useful partial report.
pub fn analyze(input: &Path, discovered: Discovery, options: AnalysisOptions) -> Report {
    let canonical_input = fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf());
    let root = if canonical_input.is_file() {
        canonical_input
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| canonical_input.clone())
    } else {
        canonical_input.clone()
    };

    let test_context_files = discover_test_context_files(&discovered.files);
    let mut files = Vec::new();
    let mut errors = Vec::new();

    for DiscoveredFile { path, is_test } in discovered.files {
        let is_test = is_test || test_context_files.contains(&path);
        let display_path = display_path(&path, &root);
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                errors.push(AnalysisError {
                    path: Some(display_path),
                    kind: ErrorKind::Read,
                    message: error.to_string(),
                });
                continue;
            }
        };

        let line_analysis = classify_lines(&source);
        let lines = line_analysis.counts.clone();
        let syntax = match syn::parse_file(&source) {
            Ok(file) => file,
            Err(error) => {
                errors.push(AnalysisError {
                    path: Some(display_path),
                    kind: ErrorKind::Parse,
                    message: error.to_string(),
                });
                continue;
            }
        };
        let lexed = match tokens::lex(&source) {
            Ok(lexed) => lexed,
            Err(error) => {
                errors.push(AnalysisError {
                    path: Some(display_path),
                    kind: ErrorKind::Lex,
                    message: error,
                });
                continue;
            }
        };

        let collected = collect_functions(&syntax, &lexed, &line_analysis, is_test, options.model);
        let burden = summarize_burden(&collected.functions);
        let macro_opacity = measure_macro_opacity(&syntax, &lexed);
        files.push(FileReport {
            path: display_path,
            lines,
            tokens: lexed.total_tokens(),
            functions: collected.functions,
            burden,
            macro_opacity,
        });
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    for file in &mut files {
        file.functions.sort_by(|left, right| {
            left.location
                .start
                .line
                .cmp(&right.location.start.line)
                .then_with(|| left.location.start.column.cmp(&right.location.start.column))
                .then_with(|| left.name.cmp(&right.name))
        });
    }

    let summary = summarize(&files);
    let macro_opacity = summarize_macro_opacity(&files);
    let analyzed_files = files.len();
    let failed_files = errors.len();
    let coverage = Coverage {
        discovered_files: analyzed_files + failed_files,
        analyzed_files,
        failed_files,
        test_files: test_context_files.len(),
        complete: failed_files == 0,
    };

    Report {
        tool: "kompass".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        model: options.model.label().to_owned(),
        root: root.to_string_lossy().into_owned(),
        summary,
        coverage,
        macro_opacity,
        files,
        errors,
    }
}

/// Resolve test-only external modules while preserving production reachability.
/// `syn` parses each file independently, so this pass builds the small module
/// graph needed to carry `cfg(test)` context across conventional Rust files.
fn discover_test_context_files(discovered: &[DiscoveredFile]) -> BTreeSet<std::path::PathBuf> {
    let known_files = discovered
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    let mut edges = BTreeMap::<std::path::PathBuf, Vec<ModuleEdge>>::new();
    let mut incoming = BTreeSet::new();
    for path in &known_files {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(syntax) = syn::parse_file(&source) else {
            continue;
        };
        let mut collector = ExternalTestModuleCollector {
            current_file: path,
            known_files: &known_files,
            test_context: false,
            files: Vec::new(),
        };
        collector.visit_file(&syntax);
        for edge in &collector.files {
            incoming.insert(edge.child.clone());
        }
        edges.insert(path.clone(), collector.files);
    }

    let mut reachability = BTreeMap::<std::path::PathBuf, Reachability>::new();
    let mut pending = VecDeque::new();
    for path in &known_files {
        let is_cargo_test_root = discovered
            .iter()
            .find(|file| file.path == *path)
            .is_some_and(|file| file.is_test);
        let state = reachability.entry(path.clone()).or_default();
        if is_cargo_test_root {
            state.test = true;
        }
        if !incoming.contains(path) && !is_cargo_test_root {
            state.production = true;
        }
        if state.production || state.test {
            pending.push_back(path.clone());
        }
    }

    while let Some(path) = pending.pop_front() {
        let state = reachability[&path];
        for edge in edges.get(&path).into_iter().flatten() {
            let child = reachability.entry(edge.child.clone()).or_default();
            let mut changed = false;
            if edge.test_only {
                if (state.production || state.test) && !child.test {
                    child.test = true;
                    changed = true;
                }
            } else {
                if state.production && !child.production {
                    child.production = true;
                    changed = true;
                }
                if state.test && !child.test {
                    child.test = true;
                    changed = true;
                }
            }
            if changed {
                pending.push_back(edge.child.clone());
            }
        }
    }

    reachability
        .into_iter()
        .filter_map(|(path, state)| (state.test && !state.production).then_some(path))
        .collect()
}

#[derive(Clone, Copy, Debug, Default)]
struct Reachability {
    production: bool,
    test: bool,
}

#[derive(Clone, Debug)]
struct ModuleEdge {
    child: std::path::PathBuf,
    test_only: bool,
}

struct ExternalTestModuleCollector<'a> {
    current_file: &'a Path,
    known_files: &'a BTreeSet<std::path::PathBuf>,
    test_context: bool,
    files: Vec<ModuleEdge>,
}

impl<'ast> Visit<'ast> for ExternalTestModuleCollector<'_> {
    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        let module_is_test = self.test_context || has_test_only_attribute(&node.attrs);
        if let Some((_, items)) = &node.content {
            let parent_test_context = self.test_context;
            self.test_context = module_is_test;
            for item in items {
                self.visit_item(item);
            }
            self.test_context = parent_test_context;
        } else if let Some(path) =
            resolve_external_module_path(self.current_file, node, self.known_files)
        {
            self.files.push(ModuleEdge {
                child: path,
                test_only: module_is_test,
            });
        }
    }
}

fn resolve_external_module_path(
    current_file: &Path,
    module: &ItemMod,
    known_files: &BTreeSet<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    let base = current_file.parent()?;
    let candidates = if let Some(relative) = module_path_attribute(&module.attrs) {
        vec![base.join(relative)]
    } else {
        let name = module.ident.to_string();
        vec![
            base.join(format!("{name}.rs")),
            base.join(&name).join("mod.rs"),
        ]
    };

    candidates.into_iter().find_map(|candidate| {
        if known_files.contains(&candidate) {
            return Some(candidate);
        }
        fs::canonicalize(candidate)
            .ok()
            .filter(|canonical| known_files.contains(canonical))
    })
}

fn module_path_attribute(attributes: &[Attribute]) -> Option<std::path::PathBuf> {
    attributes.iter().find_map(|attribute| {
        if !attribute.path().is_ident("path") {
            return None;
        }
        let Meta::NameValue(name_value) = &attribute.meta else {
            return None;
        };
        let syn::Expr::Lit(expression) = &name_value.value else {
            return None;
        };
        let syn::Lit::Str(literal) = &expression.lit else {
            return None;
        };
        Some(std::path::PathBuf::from(literal.value()))
    })
}

fn summarize(files: &[FileReport]) -> Summary {
    let mut summary = Summary {
        files: files.len(),
        ..Summary::default()
    };
    let mut production = Vec::new();
    let mut test = Vec::new();

    for file in files {
        summary.code_lines += file.lines.code;
        summary.tokens += file.tokens;
        for function in &file.functions {
            match function.category {
                Category::Production => production.push(function),
                Category::Test => test.push(function),
            }
        }
    }

    summary.production = summarize_category(&production);
    summary.test = summarize_category(&test);
    summary.burden = Burden {
        production: summary.production.total_score,
        test: summary.test.total_score,
        total: summary
            .production
            .total_score
            .saturating_add(summary.test.total_score),
    };
    summary
}

fn summarize_category(functions: &[&FunctionReport]) -> CategorySummary {
    let mut summary = CategorySummary {
        functions: functions.len(),
        ..CategorySummary::default()
    };
    let mut scores = functions
        .iter()
        .map(|function| function.score.value)
        .collect::<Vec<_>>();
    summary.total_score = scores.iter().sum();
    if !scores.is_empty() {
        scores.sort_unstable();
        summary.average_score = summary.total_score as f64 / scores.len() as f64;
        summary.highest_score = *scores.last().unwrap_or(&0);
        let index = ((scores.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
        summary.p95_score = scores[index];
    }
    summary
}

fn summarize_burden(functions: &[FunctionReport]) -> Burden {
    let mut burden = Burden::default();
    for function in functions {
        match function.category {
            Category::Production => {
                burden.production = burden.production.saturating_add(function.score.value)
            }
            Category::Test => burden.test = burden.test.saturating_add(function.score.value),
        }
    }
    burden.total = burden.production.saturating_add(burden.test);
    burden
}

fn summarize_macro_opacity(files: &[FileReport]) -> MacroOpacity {
    let mut opacity = MacroOpacity::default();
    for file in files {
        opacity.invocations = opacity
            .invocations
            .saturating_add(file.macro_opacity.invocations);
        opacity.source_tokens = opacity
            .source_tokens
            .saturating_add(file.macro_opacity.source_tokens);
    }
    opacity
}

/// Count source macro invocations whose spans are available in syn. Macro
/// definitions are excluded: they describe expansion rules rather than an
/// opaque invocation in the analyzed program. The token count makes the
/// unexpanded source area visible without pretending it is a complexity cost.
fn measure_macro_opacity(syntax: &syn::File, lexed: &LexedSource) -> MacroOpacity {
    let mut collector = MacroOpacityCollector {
        lexed,
        opacity: MacroOpacity::default(),
    };
    collector.visit_file(syntax);
    collector.opacity
}

struct MacroOpacityCollector<'a> {
    lexed: &'a LexedSource,
    opacity: MacroOpacity,
}

impl<'ast> Visit<'ast> for MacroOpacityCollector<'_> {
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if !node.path.is_ident("macro_rules") && !node.path.is_ident("macro_rules_attribute") {
            let span = node.span();
            let start = span.start();
            let end = span.end();
            self.opacity.invocations = self.opacity.invocations.saturating_add(1);
            self.opacity.source_tokens =
                self.opacity
                    .source_tokens
                    .saturating_add(self.lexed.tokens_in(
                        TokenPosition {
                            line: start.line,
                            column: start.column,
                        },
                        TokenPosition {
                            line: end.line,
                            column: end.column,
                        },
                    ));
        }
        visit::visit_macro(self, node);
    }
}

struct Collection {
    functions: Vec<FunctionReport>,
}

fn collect_functions(
    syntax: &syn::File,
    lexed: &LexedSource,
    line_analysis: &LineAnalysis,
    file_is_test: bool,
    model: ScoringModel,
) -> Collection {
    let mut collector = FunctionCollector {
        lexed,
        line_analysis,
        test_context: file_is_test,
        scopes: Vec::new(),
        function_depth: 0,
        model,
        functions: Vec::new(),
    };
    collector.visit_file(syntax);
    Collection {
        functions: collector.functions,
    }
}

struct FunctionCollector<'a> {
    lexed: &'a LexedSource,
    line_analysis: &'a LineAnalysis,
    test_context: bool,
    scopes: Vec<String>,
    function_depth: usize,
    model: ScoringModel,
    functions: Vec<FunctionReport>,
}

impl<'ast> Visit<'ast> for FunctionCollector<'_> {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let name = self.qualified_name(&node.sig.ident.to_string());
        let category = if self.test_context || has_test_only_attribute(&node.attrs) {
            Category::Test
        } else {
            Category::Production
        };
        let kind = if self.function_depth > 0 {
            FunctionKind::NestedFunction
        } else {
            FunctionKind::Function
        };
        self.functions.push(make_function_report(
            name,
            kind,
            category,
            &node.sig,
            &node.block,
            inherited_or_public_span(&node.vis, node.sig.span()),
            FunctionSource {
                lexed: self.lexed,
                line_analysis: self.line_analysis,
                model: self.model,
            },
        ));

        let parent_test_context = self.test_context;
        self.test_context = parent_test_context || matches!(category, Category::Test);
        self.scopes.push(node.sig.ident.to_string());
        self.function_depth += 1;
        visit::visit_item_fn(self, node);
        self.function_depth -= 1;
        self.scopes.pop();
        self.test_context = parent_test_context;
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if let Some((_, items)) = &node.content {
            let parent_test_context = self.test_context;
            self.test_context = parent_test_context || has_test_only_attribute(&node.attrs);
            self.scopes.push(node.ident.to_string());
            for item in items {
                self.visit_item(item);
            }
            self.scopes.pop();
            self.test_context = parent_test_context;
        }
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let parent_test_context = self.test_context;
        self.test_context = parent_test_context || has_test_only_attribute(&node.attrs);
        self.scopes.push(type_name(&node.self_ty));
        visit::visit_item_impl(self, node);
        self.scopes.pop();
        self.test_context = parent_test_context;
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        let parent_test_context = self.test_context;
        self.test_context = parent_test_context || has_test_only_attribute(&node.attrs);
        self.scopes.push(node.ident.to_string());
        visit::visit_item_trait(self, node);
        self.scopes.pop();
        self.test_context = parent_test_context;
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let name = self.qualified_name(&node.sig.ident.to_string());
        let category = if self.test_context || has_test_only_attribute(&node.attrs) {
            Category::Test
        } else {
            Category::Production
        };
        self.functions.push(make_function_report(
            name,
            FunctionKind::Method,
            category,
            &node.sig,
            &node.block,
            inherited_or_public_span(&node.vis, node.sig.span()),
            FunctionSource {
                lexed: self.lexed,
                line_analysis: self.line_analysis,
                model: self.model,
            },
        ));

        let parent_test_context = self.test_context;
        self.test_context = parent_test_context || matches!(category, Category::Test);
        self.scopes.push(node.sig.ident.to_string());
        self.function_depth += 1;
        visit::visit_impl_item_fn(self, node);
        self.function_depth -= 1;
        self.scopes.pop();
        self.test_context = parent_test_context;
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        let category = if self.test_context || has_test_only_attribute(&node.attrs) {
            Category::Test
        } else {
            Category::Production
        };
        if let Some(block) = &node.default {
            let name = self.qualified_name(&node.sig.ident.to_string());
            self.functions.push(make_function_report(
                name,
                FunctionKind::TraitMethod,
                category,
                &node.sig,
                block,
                node.sig.span(),
                FunctionSource {
                    lexed: self.lexed,
                    line_analysis: self.line_analysis,
                    model: self.model,
                },
            ));
        }

        let parent_test_context = self.test_context;
        self.test_context = parent_test_context || matches!(category, Category::Test);
        self.scopes.push(node.sig.ident.to_string());
        self.function_depth += 1;
        visit::visit_trait_item_fn(self, node);
        self.function_depth -= 1;
        self.scopes.pop();
        self.test_context = parent_test_context;
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        if self.model == ScoringModel::StructuralV1 {
            // v1 never exposed closures as function records. Preserve its
            // existing callable inventory while retaining discovery of any
            // named items nested in a closure body.
            visit::visit_expr_closure(self, node);
            return;
        }
        let start = node.span().start();
        let name = self.qualified_name(&format!("<closure@{}:{}>", start.line, start.column + 1));
        let code_lines = self
            .line_analysis
            .code_lines_in_range(start.line, node.span().end().line);
        self.functions.push(make_closure_report(
            name,
            if self.test_context {
                Category::Test
            } else {
                Category::Production
            },
            node,
            code_lines,
            FunctionSource {
                lexed: self.lexed,
                line_analysis: self.line_analysis,
                model: self.model,
            },
        ));

        // Descend solely to discover nested callables. The scoring visitor
        // used for the containing callable treats this body as opaque.
        self.scopes
            .push(format!("<closure@{}:{}>", start.line, start.column + 1));
        visit::visit_expr_closure(self, node);
        self.scopes.pop();
    }
}

impl FunctionCollector<'_> {
    fn qualified_name(&self, name: &str) -> String {
        let mut parts = self.scopes.clone();
        parts.push(name.to_owned());
        parts.join("::")
    }
}

#[derive(Clone, Copy)]
struct FunctionSource<'a> {
    lexed: &'a LexedSource,
    line_analysis: &'a LineAnalysis,
    model: ScoringModel,
}

fn make_function_report(
    name: String,
    kind: FunctionKind,
    category: Category,
    signature: &syn::Signature,
    block: &syn::Block,
    start_span: proc_macro2::Span,
    function_source: FunctionSource<'_>,
) -> FunctionReport {
    let model = function_source.model;
    let start = start_span.start();
    let end = block.span().end();
    let start_position = Position {
        line: start.line,
        column: start.column + 1,
    };
    let end_position = Position {
        line: end.line,
        column: end.column + 1,
    };
    let lines = end.line.saturating_sub(start.line) + 1;
    let parameters = signature.inputs.len();
    let explicit_parameters = explicit_parameter_count(signature);
    let code_lines = function_source
        .line_analysis
        .code_lines_in_range(start.line, end.line);
    let mut metrics = match model {
        ScoringModel::StructuralV1 => score::measure(block, code_lines, parameters),
        ScoringModel::StructuralV2 | ScoringModel::StructuralV3 => {
            score::measure_exclusive(block, code_lines, parameters, explicit_parameters)
        }
    };
    metrics.mutations += mutable_parameter_count(signature);
    let token_count = function_source.lexed.tokens_in(
        TokenPosition {
            line: start.line,
            column: start.column,
        },
        TokenPosition {
            line: end.line,
            column: end.column,
        },
    );

    FunctionReport {
        name,
        kind,
        category,
        location: Location {
            start: start_position,
            end: end_position,
        },
        lines,
        tokens: token_count,
        score: score::score_for_model(&metrics, model),
        metrics,
    }
}

fn make_closure_report(
    name: String,
    category: Category,
    closure: &syn::ExprClosure,
    code_lines: usize,
    function_source: FunctionSource<'_>,
) -> FunctionReport {
    let model = function_source.model;
    let span = closure.span();
    let start = span.start();
    let end = span.end();
    let parameters = closure.inputs.len();
    let metrics = score::measure_closure(&closure.body, code_lines, parameters);
    let token_count = function_source.lexed.tokens_in(
        TokenPosition {
            line: start.line,
            column: start.column,
        },
        TokenPosition {
            line: end.line,
            column: end.column,
        },
    );

    FunctionReport {
        name,
        kind: FunctionKind::Closure,
        category,
        location: Location {
            start: Position {
                line: start.line,
                column: start.column + 1,
            },
            end: Position {
                line: end.line,
                column: end.column + 1,
            },
        },
        lines: end.line.saturating_sub(start.line) + 1,
        tokens: token_count,
        score: score::score_for_model(&metrics, model),
        metrics,
    }
}

fn inherited_or_public_span(
    visibility: &syn::Visibility,
    signature_span: proc_macro2::Span,
) -> proc_macro2::Span {
    match visibility {
        syn::Visibility::Inherited => signature_span,
        _ => visibility
            .span()
            .join(signature_span)
            .unwrap_or(signature_span),
    }
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .unwrap_or_else(|| "_".to_owned()),
        syn::Type::Reference(reference) => type_name(&reference.elem),
        syn::Type::Tuple(tuple) => format!("tuple{}", tuple.elems.len()),
        _ => "_".to_owned(),
    }
}

fn mutable_parameter_count(signature: &syn::Signature) -> usize {
    signature
        .inputs
        .iter()
        .map(|input| match input {
            FnArg::Receiver(receiver) => usize::from(receiver.mutability.is_some()),
            FnArg::Typed(typed) => {
                let mut counter = MutablePatternCounter::default();
                counter.visit_pat(&typed.pat);
                counter.count
            }
        })
        .sum()
}

fn explicit_parameter_count(signature: &syn::Signature) -> usize {
    signature
        .inputs
        .iter()
        .filter(|input| !matches!(input, FnArg::Receiver(_)))
        .count()
}

#[derive(Default)]
struct MutablePatternCounter {
    count: usize,
}

impl<'ast> Visit<'ast> for MutablePatternCounter {
    fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
        self.count += usize::from(node.mutability.is_some());
        visit::visit_pat_ident(self, node);
    }
}

fn has_test_only_attribute(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        let path = attribute.path();
        if path.segments.last().is_some_and(|segment| {
            segment.ident == "test" || segment.ident == "rstest" || segment.ident == "test_case"
        }) {
            return true;
        }
        if !path.is_ident("cfg") {
            return false;
        }
        let Meta::List(list) = &attribute.meta else {
            return false;
        };
        let Ok(predicates) = syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated
            .parse2(list.tokens.clone())
        else {
            return false;
        };
        predicates.len() == 1 && predicates.first().is_some_and(cfg_predicate_is_test_only)
    })
}

fn cfg_predicate_is_test_only_list(
    path: &syn::Path,
    predicates: &syn::punctuated::Punctuated<Meta, Token![,]>,
) -> bool {
    if path.is_ident("all") {
        predicates.iter().any(cfg_predicate_is_test_only)
    } else if path.is_ident("any") {
        !predicates.is_empty() && predicates.iter().all(cfg_predicate_is_test_only)
    } else {
        false
    }
}

fn cfg_predicate_is_test_only(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) => {
            let Ok(predicates) = syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
            else {
                return false;
            };
            cfg_predicate_is_test_only_list(&list.path, &predicates)
        }
        Meta::NameValue(_) => false,
    }
}

fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[derive(Clone, Debug, Default)]
struct LineAnalysis {
    counts: LineCounts,
    code_prefix: Vec<usize>,
}

impl LineAnalysis {
    fn code_lines_in_range(&self, start_line: usize, end_line: usize) -> usize {
        let Some(last) = self.code_prefix.len().checked_sub(1) else {
            return 0;
        };
        let start = start_line.saturating_sub(1).min(last);
        let end = end_line.min(last);
        if end < start {
            0
        } else {
            self.code_prefix[end].saturating_sub(self.code_prefix[start])
        }
    }
}

/// Classify each physical source line in one lexical pass. A line containing
/// any code wins over a line comment, while comment-only and whitespace-only
/// lines remain distinct. The prefix table makes each function's code-line
/// lookup constant time after this file-level pass.
fn classify_lines(source: &str) -> LineAnalysis {
    let bytes = source.as_bytes();
    let mut analysis = LineAnalysis {
        code_prefix: vec![0],
        ..LineAnalysis::default()
    };
    let mut index = 0;
    let mut line_code = false;
    let mut line_comment = false;
    let mut in_line_comment = false;
    let mut block_comment_depth = 0;
    let mut raw_hashes = None;
    let mut quote = None;
    let mut escaped = false;

    while index < bytes.len() {
        if let Some(hashes) = raw_hashes {
            line_code = true;
            if bytes[index] == b'"' && tokens::has_hashes(bytes, index + 1, hashes) {
                index += 1;
                for _ in 0..hashes {
                    if index < bytes.len() {
                        line_code = true;
                        index += 1;
                    }
                }
                raw_hashes = None;
                continue;
            }
            if bytes[index] == b'\n' {
                finish_line(&mut analysis, &mut line_code, &mut line_comment);
            }
            index += 1;
            continue;
        }

        if let Some(quote_character) = quote {
            line_code = true;
            if escaped {
                escaped = false;
            } else if bytes[index] == b'\\' {
                escaped = true;
            } else if bytes[index] == quote_character {
                quote = None;
            }
            if bytes[index] == b'\n' {
                finish_line(&mut analysis, &mut line_code, &mut line_comment);
            }
            index += 1;
            continue;
        }

        if in_line_comment {
            if bytes[index] == b'\n' {
                in_line_comment = false;
                finish_line(&mut analysis, &mut line_code, &mut line_comment);
            }
            index += 1;
            continue;
        }

        if block_comment_depth > 0 {
            line_comment = true;
            if bytes[index..].starts_with(b"/*") {
                block_comment_depth += 1;
                index += 2;
                continue;
            }
            if bytes[index..].starts_with(b"*/") {
                block_comment_depth -= 1;
                index += 2;
                continue;
            }
            if bytes[index] == b'\n' {
                finish_line(&mut analysis, &mut line_code, &mut line_comment);
            }
            index += 1;
            continue;
        }

        if bytes[index] == b'\n' {
            finish_line(&mut analysis, &mut line_code, &mut line_comment);
            index += 1;
            continue;
        }

        if let Some((opening_length, hashes)) = tokens::raw_string_delimiter(bytes, index) {
            line_code = true;
            index += opening_length;
            raw_hashes = Some(hashes);
            continue;
        }
        if bytes[index] == b'"' {
            line_code = true;
            quote = Some(b'"');
            index += 1;
            continue;
        }
        if bytes[index] == b'\'' && tokens::looks_like_char_literal(bytes, index) {
            line_code = true;
            quote = Some(b'\'');
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            line_comment = true;
            block_comment_depth = 1;
            index += 2;
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            line_comment = true;
            in_line_comment = true;
            index += 2;
            continue;
        }
        if !bytes[index].is_ascii_whitespace() {
            line_code = true;
        }
        index += 1;
    }

    if !source.is_empty() && !source.ends_with('\n') {
        finish_line(&mut analysis, &mut line_code, &mut line_comment);
    }
    analysis
}

fn finish_line(analysis: &mut LineAnalysis, line_code: &mut bool, line_comment: &mut bool) {
    analysis.counts.total += 1;
    if *line_code {
        analysis.counts.code += 1;
    } else if *line_comment {
        analysis.counts.comments += 1;
    } else {
        analysis.counts.blank += 1;
    }
    let previous = *analysis.code_prefix.last().unwrap_or(&0);
    analysis
        .code_prefix
        .push(previous + usize::from(*line_code));
    *line_code = false;
    *line_comment = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(source: &str) -> Report {
        let lexed = tokens::lex(source).unwrap();
        let syntax = syn::parse_file(source).unwrap();
        let line_analysis = classify_lines(source);
        let collection = collect_functions(
            &syntax,
            &lexed,
            &line_analysis,
            false,
            ScoringModel::StructuralV1,
        );
        let file = FileReport {
            path: "src/lib.rs".to_owned(),
            lines: line_analysis.counts.clone(),
            tokens: lexed.total_tokens(),
            functions: collection.functions,
            burden: Burden::default(),
            macro_opacity: MacroOpacity::default(),
        };
        let summary = summarize(std::slice::from_ref(&file));
        Report {
            tool: "kompass".to_owned(),
            version: "0.1.0".to_owned(),
            model: ScoringModel::StructuralV1.label().to_owned(),
            root: "/tmp".to_owned(),
            summary,
            coverage: Coverage {
                discovered_files: 1,
                analyzed_files: 1,
                complete: true,
                ..Coverage::default()
            },
            macro_opacity: MacroOpacity::default(),
            files: vec![file],
            errors: Vec::new(),
        }
    }

    #[test]
    fn analysis_options_default_to_structural_v3() {
        assert_eq!(AnalysisOptions::default().model, ScoringModel::StructuralV3);
    }

    #[test]
    fn collects_modules_methods_traits_and_nested_functions() {
        let source = r#"
            mod parser {
                pub fn parse(input: &str) {
                    fn helper(value: &str) { let _ = value; }
                    let _ = helper;
                }
                struct Thing;
                impl Thing {
                    fn run(&self) {}
                }
                trait Parse {
                    fn defaulted(&self) {}
                    fn required(&self);
                }
            }
        "#;
        let result = report(source);
        let names = result.files[0]
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"parser::parse"));
        assert!(names.contains(&"parser::parse::helper"));
        assert!(names.contains(&"parser::Thing::run"));
        assert!(names.contains(&"parser::Parse::defaulted"));
        assert!(!names.contains(&"parser::Parse::required"));

        let outer = result.files[0]
            .functions
            .iter()
            .find(|function| function.name == "parser::parse")
            .unwrap();
        let nested = result.files[0]
            .functions
            .iter()
            .find(|function| function.name == "parser::parse::helper")
            .unwrap();
        assert!(outer.tokens > nested.tokens);
    }

    #[test]
    fn test_items_are_classified_without_changing_the_score_model() {
        let source = r#"
            #[cfg(test)]
            mod tests { fn module_test() {} }
            #[test]
            fn direct_test() {}
            #[tokio::test]
            async fn async_test() {}
            #[rstest]
            fn parameterized_test() {}
            #[cfg(any(test, feature = "optional"))]
            fn conditional_production() {}
            #[cfg(all(test, feature = "optional"))]
            fn conditional_test() {}
            #[cfg_attr(test, inline)]
            fn attribute_only_production() {}
            fn production() {}
        "#;
        let lexed = tokens::lex(source).unwrap();
        let syntax = syn::parse_file(source).unwrap();
        let line_analysis = classify_lines(source);
        let collection = collect_functions(
            &syntax,
            &lexed,
            &line_analysis,
            false,
            ScoringModel::StructuralV1,
        );
        assert_eq!(collection.functions.len(), 8);
        assert_eq!(
            collection
                .functions
                .iter()
                .filter(|function| function.category == Category::Test)
                .count(),
            5
        );

        let integration = collect_functions(
            &syntax,
            &lexed,
            &line_analysis,
            true,
            ScoringModel::StructuralV1,
        );
        assert!(
            integration
                .functions
                .iter()
                .all(|function| function.category == Category::Test)
        );
    }

    #[test]
    fn v2_reports_closures_and_uses_exclusive_file_burden() {
        let source = r#"
            struct Runner;
            impl Runner {
                fn run(&mut self, value: bool) {
                    let check = || if value { work(); } else { recover(); };
                    check();
                }
            }
        "#;
        let lexed = tokens::lex(source).unwrap();
        let syntax = syn::parse_file(source).unwrap();
        let line_analysis = classify_lines(source);
        let collection = collect_functions(
            &syntax,
            &lexed,
            &line_analysis,
            false,
            ScoringModel::StructuralV2,
        );
        let closure = collection
            .functions
            .iter()
            .find(|function| function.kind == FunctionKind::Closure)
            .unwrap();
        let method = collection
            .functions
            .iter()
            .find(|function| function.name == "Runner::run")
            .unwrap();

        assert_eq!(method.metrics.explicit_parameters, 1);
        assert_eq!(method.metrics.control_decisions, 0);
        assert_eq!(method.metrics.call_sites, 1);
        assert_eq!(closure.metrics.control_decisions, 1);
        assert_eq!(closure.metrics.call_sites, 2);
        assert_eq!(closure.score.value, 10 + 10 + 2 + 2 + 2 + 1);

        let burden = summarize_burden(&collection.functions);
        assert_eq!(burden.production, method.score.value + closure.score.value);
        assert_eq!(burden.total, burden.production);
    }

    #[test]
    fn macro_opacity_counts_source_invocations_and_tokens() {
        let source = r#"
            macro_rules! make_item { ($item:item) => { $item }; }
            make_item!(fn generated() {});
            fn run() {
                println!("hello");
                let values = vec![1, 2, 3];
                let _ = values;
            }
        "#;
        let lexed = tokens::lex(source).unwrap();
        let syntax = syn::parse_file(source).unwrap();
        let opacity = measure_macro_opacity(&syntax, &lexed);

        assert_eq!(opacity.invocations, 3);
        assert!(opacity.source_tokens >= opacity.invocations);
    }

    #[test]
    fn cfg_test_external_modules_inherit_test_category() {
        let root = std::env::temp_dir().join(format!(
            "kompass-test-modules-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let lib_path = root.join("lib.rs");
        let helper_path = root.join("test_helpers.rs");
        let nested_path = root.join("nested.rs");
        std::fs::write(
            &lib_path,
            "#[cfg(test)] mod test_helpers;\nfn production() {}\n",
        )
        .unwrap();
        std::fs::write(&helper_path, "mod nested;\nfn helper() {}\n").unwrap();
        std::fs::write(&nested_path, "fn nested() {}\n").unwrap();

        let report = analyze(
            &root,
            Discovery {
                files: vec![
                    DiscoveredFile {
                        path: lib_path,
                        is_test: false,
                    },
                    DiscoveredFile {
                        path: helper_path,
                        is_test: false,
                    },
                    DiscoveredFile {
                        path: nested_path,
                        is_test: false,
                    },
                ],
                test_files: 0,
            },
            AnalysisOptions::default(),
        );

        assert_eq!(report.summary.production.functions, 1);
        assert_eq!(report.summary.test.functions, 2);
        assert_eq!(report.coverage.test_files, 2);
        assert!(report.files.iter().all(|file| {
            file.path == "lib.rs" && file.functions[0].category == Category::Production
                || file.path != "lib.rs"
                    && file
                        .functions
                        .iter()
                        .all(|function| function.category == Category::Test)
        }));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_external_module_is_production_when_reachable_from_both_contexts() {
        let root = std::env::temp_dir().join(format!(
            "kompass-shared-modules-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let lib_path = root.join("lib.rs");
        let shared_path = root.join("shared.rs");
        let test_only_path = root.join("test_only.rs");
        std::fs::write(
            &lib_path,
            "mod shared;\n#[cfg(test)]\n#[path = \"shared.rs\"]\nmod shared_for_tests;\n#[cfg(test)]\nmod test_only;\nfn production() {}\n",
        )
        .unwrap();
        std::fs::write(&shared_path, "fn shared() {}\n").unwrap();
        std::fs::write(&test_only_path, "fn helper() {}\n").unwrap();

        let report = analyze(
            &root,
            Discovery {
                files: vec![
                    DiscoveredFile {
                        path: lib_path,
                        is_test: false,
                    },
                    DiscoveredFile {
                        path: shared_path,
                        is_test: false,
                    },
                    DiscoveredFile {
                        path: test_only_path,
                        is_test: false,
                    },
                ],
                test_files: 0,
            },
            AnalysisOptions::default(),
        );

        assert_eq!(report.summary.production.functions, 2);
        assert_eq!(report.summary.test.functions, 1);
        assert_eq!(report.coverage.test_files, 1);
        let shared = report
            .files
            .iter()
            .find(|file| file.path == "shared.rs")
            .unwrap();
        assert_eq!(shared.functions[0].category, Category::Production);
        let test_only = report
            .files
            .iter()
            .find(|file| file.path == "test_only.rs")
            .unwrap();
        assert_eq!(test_only.functions[0].category, Category::Test);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn function_tokens_cover_signature_through_body_but_exclude_outer_attrs() {
        let source = r#"
            #[inline]
            fn outer<'a>(value: &'a str) {
                fn inner() {}
                let _ = value;
            }
        "#;
        let lexed = tokens::lex(source).unwrap();
        let syntax = syn::parse_file(source).unwrap();
        let line_analysis = classify_lines(source);
        let collection = collect_functions(
            &syntax,
            &lexed,
            &line_analysis,
            false,
            ScoringModel::StructuralV1,
        );
        let outer = collection
            .functions
            .iter()
            .find(|function| function.name == "outer")
            .unwrap();
        let inner = collection
            .functions
            .iter()
            .find(|function| function.name == "outer::inner")
            .unwrap();

        assert_eq!(
            outer.tokens,
            tokens::lex("fn outer<'a>(value: &'a str) { fn inner() {} let _ = value; }")
                .unwrap()
                .total_tokens()
        );
        assert_eq!(
            inner.tokens,
            tokens::lex("fn inner() {}").unwrap().total_tokens()
        );
    }

    #[test]
    fn line_counts_distinguish_code_comments_and_blank_lines() {
        let counts = classify_lines("fn f() {}\n\n// comment\n/* block\n * comment\n */\n").counts;
        assert_eq!(counts.total, 6);
        assert_eq!(counts.code, 1);
        assert_eq!(counts.comments, 4);
        assert_eq!(counts.blank, 1);
    }

    #[test]
    fn line_counts_handle_nested_comments_and_literal_markers() {
        let source = concat!(
            "let text = \"/* not comment */ //\"; // trailing\n",
            "/* outer\n",
            "   /* nested */\n",
            "   still */\n",
            "let raw = r###\"// not comment\n",
            "still\"###;\n",
            "let character = 'λ';\n",
        );
        let counts = classify_lines(source).counts;

        assert_eq!(counts.total, 7);
        assert_eq!(counts.code, 4);
        assert_eq!(counts.comments, 3);
        assert_eq!(counts.blank, 0);
    }

    #[test]
    fn locations_are_one_based_and_public_visibility_is_included() {
        let source =
            "\n\npub fn free() {}\n\nstruct Thing;\nimpl Thing {\n    pub fn method() {}\n}\n";
        let result = report(source);
        let free = result.files[0]
            .functions
            .iter()
            .find(|function| function.name == "free")
            .unwrap();
        let method = result.files[0]
            .functions
            .iter()
            .find(|function| function.name == "Thing::method")
            .unwrap();

        assert_eq!(free.location.start, Position { line: 3, column: 1 });
        assert_eq!(
            free.location.end,
            Position {
                line: 3,
                column: 17
            }
        );
        assert_eq!(method.location.start, Position { line: 7, column: 5 });
        assert_eq!(
            method.location.end,
            Position {
                line: 7,
                column: 23
            }
        );
        assert_eq!(free.metrics.code_lines, 1);
        assert_eq!(method.metrics.code_lines, 1);
        assert_eq!(
            free.tokens,
            tokens::lex("pub fn free() {}").unwrap().total_tokens()
        );
        assert_eq!(
            method.tokens,
            tokens::lex("pub fn method() {}").unwrap().total_tokens()
        );
    }

    #[test]
    fn an_unparseable_file_is_reported_as_partial_coverage() {
        let path = std::env::temp_dir().join(format!(
            "kompass-invalid-{}-{}.rs",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "fn broken( {").unwrap();

        let report = analyze(
            &path,
            Discovery {
                files: vec![DiscoveredFile {
                    path: path.clone(),
                    is_test: false,
                }],
                test_files: 0,
            },
            AnalysisOptions::default(),
        );

        assert!(!report.coverage.complete);
        assert_eq!(report.coverage.failed_files, 1);
        assert!(matches!(report.errors[0].kind, ErrorKind::Parse));
        std::fs::remove_file(path).unwrap();
    }
}
