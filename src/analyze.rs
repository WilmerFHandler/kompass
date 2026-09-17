use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, FnArg, ItemConst, ItemFn, ItemMod, ItemStatic, Meta, Token};

use crate::discover::{DiscoveredFile, Discovery};
use crate::identity;
use crate::model::{
    AnalysisContract, AnalysisError, Burden, Category, CategorySummary, Coverage, ErrorKind,
    FileAnalysis, FileReport, FunctionKind, FunctionReport, Language, LineCounts, Location,
    MacroOpacity, Position, Report, SCORE_MODEL, Summary,
};
use crate::score;
use crate::tokens::{self, LexedSource, TokenPosition};

/// Callback contract for the Python frontend. The frontend owns parsing and
/// metric collection; the analyzer owns path/category metadata and aggregate
/// reporting. A string error is converted to a parse error for the file.
pub type PythonAnalyzer =
    fn(path: &Path, source: &str, category: Category) -> Result<FileAnalysis, String>;

/// Analyze each discovered source file independently. A file that cannot be
/// read, lexed, or parsed is recorded as an error while successful files still
/// produce a useful partial report.
pub fn analyze(input: &Path, discovered: Discovery) -> Report {
    analyze_with_frontends(input, discovered, Some(crate::python::analyze_file))
}

/// Analyze each discovered file with an optional Python frontend. Keeping the
/// callback at this seam lets the Ruff-backed `python` module evolve without
/// coupling discovery, reporting, or the Rust frontend to its AST types.
pub fn analyze_with_frontends(
    input: &Path,
    discovered: Discovery,
    python_analyzer: Option<PythonAnalyzer>,
) -> Report {
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
    let mut test_files = discovered
        .files
        .iter()
        .filter(|file| file.category == Category::Test)
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    test_files.extend(test_context_files.iter().cloned());
    let mut files = Vec::new();
    let mut errors = Vec::new();

    for DiscoveredFile {
        path,
        language,
        category,
    } in discovered.files
    {
        // Rust module reachability can promote a discovered source file to a
        // test-only unit. Python classification comes entirely from discovery.
        let category = if language == Language::Rust && test_context_files.contains(&path) {
            Category::Test
        } else {
            category
        };
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

        let file_analysis = match language {
            Language::Rust => analyze_rust_file(&source, category == Category::Test),
            Language::Python => match python_analyzer {
                Some(analyzer) => {
                    analyzer(&path, &source, category).map_err(|message| AnalysisError {
                        path: None,
                        kind: ErrorKind::Parse,
                        message,
                    })
                }
                None => Err(AnalysisError {
                    path: None,
                    kind: ErrorKind::Parse,
                    message:
                        "Python frontend is unavailable; build Kompass with the Python frontend"
                            .to_owned(),
                }),
            },
        };
        let file_analysis = match file_analysis {
            Ok(file_analysis) => file_analysis,
            Err(mut error) => {
                error.path = Some(display_path);
                errors.push(error);
                continue;
            }
        };
        let mut file_analysis = file_analysis;
        identity::assign_snapshot_ids(&display_path, language, &mut file_analysis.functions);
        let burden = summarize_burden(&file_analysis.functions);
        files.push(FileReport {
            path: display_path,
            language,
            lines: file_analysis.lines,
            tokens: file_analysis.tokens,
            functions: file_analysis.functions,
            burden,
            macro_opacity: file_analysis.macro_opacity,
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
        test_files: test_files.len(),
        complete: failed_files == 0,
    };

    Report {
        report_kind: "analysis".to_owned(),
        schema_version: crate::model::SCHEMA_VERSION,
        tool: "kompass".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        evidence_version: identity::EVIDENCE_VERSION.to_owned(),
        model: SCORE_MODEL.to_owned(),
        analysis_contract: AnalysisContract::current(),
        root: root.to_string_lossy().into_owned(),
        summary,
        coverage,
        macro_opacity,
        files,
        errors,
    }
}

fn analyze_rust_file(source: &str, file_is_test: bool) -> Result<FileAnalysis, AnalysisError> {
    let line_analysis = classify_lines(source);
    let syntax = syn::parse_file(source).map_err(|error| AnalysisError {
        path: None,
        kind: ErrorKind::Parse,
        message: error.to_string(),
    })?;
    let lexed = tokens::lex(source).map_err(|error| AnalysisError {
        path: None,
        kind: ErrorKind::Lex,
        message: error,
    })?;
    let collected = collect_functions(source, &syntax, &lexed, &line_analysis, file_is_test);
    Ok(FileAnalysis {
        lines: line_analysis.counts,
        tokens: lexed.total_tokens(),
        functions: collected.functions,
        macro_opacity: measure_macro_opacity(&syntax, &lexed),
    })
}

/// Resolve test-only external modules while preserving production reachability.
/// `syn` parses each file independently, so this pass builds the small module
/// graph needed to carry `cfg(test)` context across conventional Rust files.
fn discover_test_context_files(discovered: &[DiscoveredFile]) -> BTreeSet<PathBuf> {
    let mut known_files = BTreeSet::new();
    let mut cargo_test_roots = BTreeMap::new();
    for file in discovered {
        if file.language != Language::Rust {
            continue;
        }
        let path = file.path.clone();
        known_files.insert(path.clone());
        cargo_test_roots
            .entry(path)
            .or_insert(file.category == Category::Test);
    }

    let (edges, incoming) = collect_module_edges(&known_files);
    let mut reachability = BTreeMap::<PathBuf, Reachability>::new();
    let mut pending = VecDeque::new();
    for path in &known_files {
        let is_cargo_test_root = cargo_test_roots.get(path).copied().unwrap_or(false);
        let state = Reachability {
            production: !incoming.contains(path) && !is_cargo_test_root,
            test: is_cargo_test_root,
        };
        if state.production || state.test {
            pending.push_back(path.clone());
        }
        reachability.insert(path.clone(), state);
    }

    propagate_reachability(&edges, &mut reachability, &mut pending);

    reachability
        .into_iter()
        .filter_map(|(path, state)| (state.test && !state.production).then_some(path))
        .collect()
}

fn collect_module_edges(
    known_files: &BTreeSet<PathBuf>,
) -> (BTreeMap<PathBuf, Vec<ModuleEdge>>, BTreeSet<PathBuf>) {
    let mut edges = BTreeMap::new();
    let mut incoming = BTreeSet::new();
    for path in known_files {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(syntax) = syn::parse_file(&source) else {
            continue;
        };
        let mut collector = ExternalTestModuleCollector {
            current_file: path,
            known_files,
            test_context: false,
            files: Vec::new(),
        };
        collector.visit_file(&syntax);
        for edge in &collector.files {
            incoming.insert(edge.child.clone());
        }
        edges.insert(path.clone(), collector.files);
    }
    (edges, incoming)
}

fn propagate_reachability(
    edges: &BTreeMap<PathBuf, Vec<ModuleEdge>>,
    reachability: &mut BTreeMap<PathBuf, Reachability>,
    pending: &mut VecDeque<PathBuf>,
) {
    while let Some(path) = pending.pop_front() {
        let state = reachability[&path];
        for edge in edges.get(&path).into_iter().flatten() {
            let child = reachability.entry(edge.child.clone()).or_default();
            if child.merge_from(state, edge.test_only) {
                pending.push_back(edge.child.clone());
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Reachability {
    production: bool,
    test: bool,
}

impl Reachability {
    fn merge_from(&mut self, parent: Self, test_only: bool) -> bool {
        let previous = *self;
        if test_only {
            self.test |= parent.production || parent.test;
        } else {
            self.production |= parent.production;
            self.test |= parent.test;
        }
        *self != previous
    }
}

#[derive(Clone, Debug)]
struct ModuleEdge {
    child: PathBuf,
    test_only: bool,
}

struct ExternalTestModuleCollector<'a> {
    current_file: &'a Path,
    known_files: &'a BTreeSet<PathBuf>,
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
    known_files: &BTreeSet<PathBuf>,
) -> Option<PathBuf> {
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

fn module_path_attribute(attributes: &[Attribute]) -> Option<PathBuf> {
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
        Some(PathBuf::from(literal.value()))
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
        match file.language {
            Language::Rust => summary.languages.rust = summary.languages.rust.saturating_add(1),
            Language::Python => {
                summary.languages.python = summary.languages.python.saturating_add(1)
            }
        }
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
        opacity.definitions = opacity
            .definitions
            .saturating_add(file.macro_opacity.definitions);
        opacity.definition_tokens = opacity
            .definition_tokens
            .saturating_add(file.macro_opacity.definition_tokens);
    }
    opacity
}

/// Count source macro invocations whose spans are available in syn. Macro
/// definitions are measured separately: they describe expansion rules rather
/// than opaque invocations in the analyzed program, but changing their rule
/// bodies must remain visible to a before/after review. The token counts make
/// both unexpanded source areas visible without pretending either is a
/// complexity cost.
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
    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        if node.ident.is_some() {
            let span = node.span();
            self.opacity.definitions = self.opacity.definitions.saturating_add(1);
            self.opacity.definition_tokens = self
                .opacity
                .definition_tokens
                .saturating_add(tokens_in_span(self.lexed, span));
            // Macro rule bodies are token streams, not parsed Rust items, so
            // visiting them cannot discover additional source invocations.
            for attribute in &node.attrs {
                self.visit_attribute(attribute);
            }
            return;
        }
        self.visit_macro(&node.mac);
        for attribute in &node.attrs {
            self.visit_attribute(attribute);
        }
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if !node.path.is_ident("macro_rules") && !node.path.is_ident("macro_rules_attribute") {
            self.opacity.invocations = self.opacity.invocations.saturating_add(1);
            self.opacity.source_tokens = self
                .opacity
                .source_tokens
                .saturating_add(tokens_in_span(self.lexed, node.span()));
        }
        visit::visit_macro(self, node);
    }
}

fn span_to_token_range(span: proc_macro2::Span) -> (TokenPosition, TokenPosition) {
    let start = span.start();
    let end = span.end();
    (
        TokenPosition {
            line: start.line,
            column: start.column,
        },
        TokenPosition {
            line: end.line,
            column: end.column,
        },
    )
}

fn tokens_in_span(lexed: &LexedSource, span: proc_macro2::Span) -> usize {
    let (start, end) = span_to_token_range(span);
    lexed.tokens_in(start, end)
}

struct Collection {
    functions: Vec<FunctionReport>,
}

fn collect_functions(
    source: &str,
    syntax: &syn::File,
    lexed: &LexedSource,
    line_analysis: &LineAnalysis,
    file_is_test: bool,
) -> Collection {
    let closure_depths = score::closure_base_depths(syntax)
        .into_iter()
        .map(|closure| ((closure.start, closure.end), closure.depth))
        .collect();
    let mut collector = FunctionCollector {
        source,
        lexed,
        line_analysis,
        test_context: file_is_test,
        scopes: Vec::new(),
        function_depth: 0,
        closure_depths,
        functions: Vec::new(),
    };
    collector.visit_file(syntax);
    Collection {
        functions: collector.functions,
    }
}

struct FunctionCollector<'a> {
    source: &'a str,
    lexed: &'a LexedSource,
    line_analysis: &'a LineAnalysis,
    test_context: bool,
    scopes: Vec<String>,
    function_depth: usize,
    closure_depths: BTreeMap<(TokenPosition, TokenPosition), usize>,
    functions: Vec<FunctionReport>,
}

impl<'ast> Visit<'ast> for FunctionCollector<'_> {
    fn visit_item_const(&mut self, node: &'ast ItemConst) {
        let category = if self.test_context || has_test_only_attribute(&node.attrs) {
            Category::Test
        } else {
            Category::Production
        };
        self.functions.push(make_initializer_report(
            self.qualified_name(&node.ident.to_string()),
            FunctionKind::ConstInitializer,
            category,
            &node.expr,
            node.span(),
            FunctionSource {
                source: self.source,
                lexed: self.lexed,
                line_analysis: self.line_analysis,
            },
        ));
        visit::visit_item_const(self, node);
    }

    fn visit_item_static(&mut self, node: &'ast ItemStatic) {
        let category = if self.test_context || has_test_only_attribute(&node.attrs) {
            Category::Test
        } else {
            Category::Production
        };
        self.functions.push(make_initializer_report(
            self.qualified_name(&node.ident.to_string()),
            FunctionKind::StaticInitializer,
            category,
            &node.expr,
            node.span(),
            FunctionSource {
                source: self.source,
                lexed: self.lexed,
                line_analysis: self.line_analysis,
            },
        ));
        visit::visit_item_static(self, node);
    }

    fn visit_impl_item_const(&mut self, node: &'ast syn::ImplItemConst) {
        let category = if self.test_context || has_test_only_attribute(&node.attrs) {
            Category::Test
        } else {
            Category::Production
        };
        self.functions.push(make_initializer_report(
            self.qualified_name(&node.ident.to_string()),
            FunctionKind::ConstInitializer,
            category,
            &node.expr,
            node.span(),
            FunctionSource {
                source: self.source,
                lexed: self.lexed,
                line_analysis: self.line_analysis,
            },
        ));
        visit::visit_impl_item_const(self, node);
    }

    fn visit_trait_item_const(&mut self, node: &'ast syn::TraitItemConst) {
        let category = if self.test_context || has_test_only_attribute(&node.attrs) {
            Category::Test
        } else {
            Category::Production
        };
        if let Some((_, expression)) = &node.default {
            self.functions.push(make_initializer_report(
                self.qualified_name(&node.ident.to_string()),
                FunctionKind::ConstInitializer,
                category,
                expression,
                node.span(),
                FunctionSource {
                    source: self.source,
                    lexed: self.lexed,
                    line_analysis: self.line_analysis,
                },
            ));
        }
        visit::visit_trait_item_const(self, node);
    }

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
                source: self.source,
                lexed: self.lexed,
                line_analysis: self.line_analysis,
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
                source: self.source,
                lexed: self.lexed,
                line_analysis: self.line_analysis,
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
                    source: self.source,
                    lexed: self.lexed,
                    line_analysis: self.line_analysis,
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
            self.closure_depths
                .get(&(
                    span_to_token_range(node.span()).0,
                    span_to_token_range(node.span()).1,
                ))
                .copied()
                .unwrap_or(0),
            FunctionSource {
                source: self.source,
                lexed: self.lexed,
                line_analysis: self.line_analysis,
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
    source: &'a str,
    lexed: &'a LexedSource,
    line_analysis: &'a LineAnalysis,
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
    let mut metrics = score::measure(block, code_lines, parameters, explicit_parameters);
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

    let location = Location {
        start: start_position.clone(),
        end: end_position.clone(),
    };
    FunctionReport {
        snapshot_id: String::new(),
        declaration_fingerprint: identity::rust_span_fingerprint(
            function_source.source,
            start_span,
            Language::Rust,
        ),
        body_fingerprint: identity::rust_span_fingerprint(
            function_source.source,
            block.span(),
            Language::Rust,
        ),
        name,
        kind,
        category,
        location: Location {
            start: start_position.clone(),
            end: end_position.clone(),
        },
        lines,
        tokens: token_count,
        score: score::score_with_contributions(
            &metrics,
            location.clone(),
            score::rust_function_contributions(block, signature, location),
        ),
        metrics,
    }
}

fn make_closure_report(
    name: String,
    category: Category,
    closure: &syn::ExprClosure,
    code_lines: usize,
    base_depth: usize,
    function_source: FunctionSource<'_>,
) -> FunctionReport {
    let span = closure.span();
    let start = span.start();
    let end = span.end();
    let parameters = closure.inputs.len();
    let metrics =
        score::measure_closure_at_depth(&closure.body, code_lines, parameters, base_depth);
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

    let location = Location {
        start: Position {
            line: start.line,
            column: start.column + 1,
        },
        end: Position {
            line: end.line,
            column: end.column + 1,
        },
    };
    FunctionReport {
        snapshot_id: String::new(),
        declaration_fingerprint: identity::lexical_fingerprint(
            function_source.source,
            identity::rust_offset(function_source.source, start),
            identity::rust_offset(function_source.source, closure.body.span().start()),
            Language::Rust,
        ),
        body_fingerprint: identity::rust_span_fingerprint(
            function_source.source,
            closure.body.span(),
            Language::Rust,
        ),
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
        score: score::score_with_contributions(
            &metrics,
            location.clone(),
            score::rust_closure_contributions(&closure.body, &closure.inputs, base_depth, location),
        ),
        metrics,
    }
}

fn make_initializer_report(
    name: String,
    kind: FunctionKind,
    category: Category,
    expression: &syn::Expr,
    item_span: proc_macro2::Span,
    function_source: FunctionSource<'_>,
) -> FunctionReport {
    let start = item_span.start();
    let end = item_span.end();
    let code_lines = function_source
        .line_analysis
        .code_lines_in_range(start.line, end.line);
    let metrics = score::measure_expression(expression, code_lines, 0, 0);
    let token_count = tokens_in_span(function_source.lexed, item_span);

    let location = Location {
        start: Position {
            line: start.line,
            column: start.column + 1,
        },
        end: Position {
            line: end.line,
            column: end.column + 1,
        },
    };
    FunctionReport {
        snapshot_id: String::new(),
        declaration_fingerprint: identity::lexical_fingerprint(
            function_source.source,
            identity::rust_offset(function_source.source, start),
            identity::rust_offset(function_source.source, expression.span().start()),
            Language::Rust,
        ),
        body_fingerprint: identity::rust_span_fingerprint(
            function_source.source,
            expression.span(),
            Language::Rust,
        ),
        name,
        kind,
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
        score: score::score_with_contributions(
            &metrics,
            location.clone(),
            score::rust_initializer_contributions(expression, location),
        ),
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

#[derive(Clone, Copy, Debug)]
enum LexicalState {
    Normal,
    LineComment,
    BlockComment(usize),
    Quoted { delimiter: u8, escaped: bool },
    RawString { hashes: usize },
}

#[derive(Clone, Copy, Debug, Default)]
enum LineKind {
    #[default]
    Blank,
    Comment,
    Code,
}

impl LineKind {
    fn mark_comment(&mut self) {
        *self = match *self {
            Self::Code => Self::Code,
            Self::Blank | Self::Comment => Self::Comment,
        };
    }
}

/// Classify each physical source line in one lexical pass. A line containing
/// any code wins over a line comment, while comment-only and whitespace-only
/// lines remain distinct. The prefix table makes each function's code-line
/// lookup constant time after this file-level pass.
fn classify_lines(source: &str) -> LineAnalysis {
    LineClassifier::new(source).run()
}

struct LineClassifier<'a> {
    bytes: &'a [u8],
    index: usize,
    analysis: LineAnalysis,
    line_kind: LineKind,
    state: LexicalState,
}

impl<'a> LineClassifier<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            bytes: source.as_bytes(),
            index: 0,
            analysis: LineAnalysis {
                code_prefix: vec![0],
                ..LineAnalysis::default()
            },
            line_kind: LineKind::default(),
            state: LexicalState::Normal,
        }
    }

    fn run(mut self) -> LineAnalysis {
        while self.index < self.bytes.len() {
            self.advance();
        }
        if !self.bytes.is_empty() && !self.bytes.ends_with(b"\n") {
            self.finish_line();
        }
        self.analysis
    }

    fn advance(&mut self) {
        match self.state {
            LexicalState::Normal => self.advance_normal(),
            LexicalState::LineComment => self.advance_line_comment(),
            LexicalState::BlockComment(depth) => self.advance_block_comment(depth),
            LexicalState::Quoted { delimiter, escaped } => self.advance_quoted(delimiter, escaped),
            LexicalState::RawString { hashes } => self.advance_raw_string(hashes),
        }
    }

    fn advance_normal(&mut self) {
        if self.bytes[self.index] == b'\n' {
            self.finish_line();
            self.index += 1;
            return;
        }
        if let Some((opening_length, hashes)) = tokens::raw_string_delimiter(self.bytes, self.index)
        {
            self.line_kind = LineKind::Code;
            self.state = LexicalState::RawString { hashes };
            self.index += opening_length;
            return;
        }
        if self.bytes[self.index] == b'"' {
            self.line_kind = LineKind::Code;
            self.state = LexicalState::Quoted {
                delimiter: b'"',
                escaped: false,
            };
            self.index += 1;
            return;
        }
        if self.bytes[self.index] == b'\''
            && tokens::looks_like_char_literal(self.bytes, self.index)
        {
            self.line_kind = LineKind::Code;
            self.state = LexicalState::Quoted {
                delimiter: b'\'',
                escaped: false,
            };
            self.index += 1;
            return;
        }
        if self.bytes[self.index..].starts_with(b"/*") {
            self.line_kind.mark_comment();
            self.state = LexicalState::BlockComment(1);
            self.index += 2;
            return;
        }
        if self.bytes[self.index..].starts_with(b"//") {
            self.line_kind.mark_comment();
            self.state = LexicalState::LineComment;
            self.index += 2;
            return;
        }
        if !self.bytes[self.index].is_ascii_whitespace() {
            self.line_kind = LineKind::Code;
        }
        self.index += 1;
    }

    fn advance_line_comment(&mut self) {
        if self.bytes[self.index] == b'\n' {
            self.state = LexicalState::Normal;
            self.finish_line();
        }
        self.index += 1;
    }

    fn advance_block_comment(&mut self, depth: usize) {
        self.line_kind.mark_comment();
        if self.bytes[self.index..].starts_with(b"/*") {
            self.state = LexicalState::BlockComment(depth + 1);
            self.index += 2;
        } else if self.bytes[self.index..].starts_with(b"*/") {
            self.state = if depth == 1 {
                LexicalState::Normal
            } else {
                LexicalState::BlockComment(depth - 1)
            };
            self.index += 2;
        } else {
            if self.bytes[self.index] == b'\n' {
                self.finish_line();
            }
            self.index += 1;
        }
    }

    fn advance_quoted(&mut self, delimiter: u8, escaped: bool) {
        self.line_kind = LineKind::Code;
        if escaped {
            self.state = LexicalState::Quoted {
                delimiter,
                escaped: false,
            };
        } else if self.bytes[self.index] == b'\\' {
            self.state = LexicalState::Quoted {
                delimiter,
                escaped: true,
            };
        } else if self.bytes[self.index] == delimiter {
            self.state = LexicalState::Normal;
        }
        if self.bytes[self.index] == b'\n' {
            self.finish_line();
        }
        self.index += 1;
    }

    fn advance_raw_string(&mut self, hashes: usize) {
        self.line_kind = LineKind::Code;
        if self.bytes[self.index] == b'"' && tokens::has_hashes(self.bytes, self.index + 1, hashes)
        {
            self.index += hashes + 1;
            self.state = LexicalState::Normal;
        } else {
            if self.bytes[self.index] == b'\n' {
                self.finish_line();
            }
            self.index += 1;
        }
    }

    fn finish_line(&mut self) {
        self.analysis.counts.total += 1;
        let code = match self.line_kind {
            LineKind::Code => {
                self.analysis.counts.code += 1;
                true
            }
            LineKind::Comment => {
                self.analysis.counts.comments += 1;
                false
            }
            LineKind::Blank => {
                self.analysis.counts.blank += 1;
                false
            }
        };
        let previous = *self.analysis.code_prefix.last().unwrap_or(&0);
        self.analysis.code_prefix.push(previous + usize::from(code));
        self.line_kind = LineKind::Blank;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(source: &str) -> Report {
        let lexed = tokens::lex(source).unwrap();
        let syntax = syn::parse_file(source).unwrap();
        let line_analysis = classify_lines(source);
        let collection = collect_functions(source, &syntax, &lexed, &line_analysis, false);
        let file = FileReport {
            path: "src/lib.rs".to_owned(),
            language: Language::Rust,
            lines: line_analysis.counts.clone(),
            tokens: lexed.total_tokens(),
            functions: collection.functions,
            burden: Burden::default(),
            macro_opacity: MacroOpacity::default(),
        };
        let summary = summarize(std::slice::from_ref(&file));
        Report {
            report_kind: "analysis".to_owned(),
            schema_version: crate::model::SCHEMA_VERSION,
            tool: "kompass".to_owned(),
            version: "0.1.0".to_owned(),
            evidence_version: identity::EVIDENCE_VERSION.to_owned(),
            model: SCORE_MODEL.to_owned(),
            analysis_contract: AnalysisContract::current(),
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
    fn test_items_are_classified_without_changing_the_score() {
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
        let collection = collect_functions(source, &syntax, &lexed, &line_analysis, false);
        assert_eq!(collection.functions.len(), 8);
        assert_eq!(
            collection
                .functions
                .iter()
                .filter(|function| function.category == Category::Test)
                .count(),
            5
        );

        let integration = collect_functions(source, &syntax, &lexed, &line_analysis, true);
        assert!(
            integration
                .functions
                .iter()
                .all(|function| function.category == Category::Test)
        );
    }

    #[test]
    fn reports_closures_and_uses_exclusive_file_burden() {
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
        let collection = collect_functions(source, &syntax, &lexed, &line_analysis, false);
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
        assert_eq!(closure.score.value, 10 + 10 + 2 * 2);

        let burden = summarize_burden(&collection.functions);
        assert_eq!(burden.production, method.score.value + closure.score.value);
        assert_eq!(burden.total, burden.production);
    }

    #[test]
    fn reports_const_static_and_associated_initializers() {
        let source = r#"
            const TOP: i32 = { if true { 1 } else { 2 } };
            static GLOBAL: i32 = { if false { 3 } else { 4 } };
            struct Thing;
            impl Thing {
                const ASSOCIATED: i32 = { let value = 5; value };
            }
            trait Trait {
                const DEFAULTED: i32 = { if true { 6 } else { 7 } };
                const REQUIRED: i32;
            }
            fn run() {
                const LOCAL: i32 = { if true { 8 } else { 9 } };
                let _ = LOCAL;
            }
        "#;
        let lexed = tokens::lex(source).unwrap();
        let syntax = syn::parse_file(source).unwrap();
        let line_analysis = classify_lines(source);
        let collection = collect_functions(source, &syntax, &lexed, &line_analysis, false);

        assert_eq!(collection.functions.len(), 6);
        let initializer = |name: &str, kind: FunctionKind| {
            collection
                .functions
                .iter()
                .find(|function| function.name == name && function.kind == kind)
                .unwrap_or_else(|| panic!("missing initializer {name}"))
        };

        assert_eq!(
            initializer("TOP", FunctionKind::ConstInitializer)
                .metrics
                .control_decisions,
            1
        );
        assert_eq!(
            initializer("GLOBAL", FunctionKind::StaticInitializer)
                .metrics
                .control_decisions,
            1
        );
        assert!(
            initializer("Thing::ASSOCIATED", FunctionKind::ConstInitializer)
                .metrics
                .statements
                > 0
        );
        assert_eq!(
            initializer("Trait::DEFAULTED", FunctionKind::ConstInitializer)
                .metrics
                .control_decisions,
            1
        );
        assert!(
            collection
                .functions
                .iter()
                .all(|function| function.name != "Trait::REQUIRED")
        );
        assert_eq!(
            initializer("run::LOCAL", FunctionKind::ConstInitializer)
                .metrics
                .control_decisions,
            1
        );

        let run = collection
            .functions
            .iter()
            .find(|function| function.name == "run")
            .unwrap();
        assert_eq!(run.metrics.control_decisions, 0);
    }

    #[test]
    fn closure_extraction_keeps_surrounding_nesting_in_the_child() {
        let source =
            "fn run(value: bool) { if value { let check = || if value { work(); }; check(); } }";
        let lexed = tokens::lex(source).unwrap();
        let syntax = syn::parse_file(source).unwrap();
        let line_analysis = classify_lines(source);
        let collection = collect_functions(source, &syntax, &lexed, &line_analysis, false);
        let parent = collection
            .functions
            .iter()
            .find(|function| function.name == "run")
            .unwrap();
        let closure = collection
            .functions
            .iter()
            .find(|function| function.kind == FunctionKind::Closure)
            .unwrap();

        assert_eq!(parent.metrics.control_decisions, 1);
        assert_eq!(parent.metrics.nesting_penalty, 0);
        assert_eq!(closure.metrics.control_decisions, 1);
        assert_eq!(closure.metrics.nesting_penalty, 1);
        assert_eq!(closure.metrics.call_sites, 1);
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
        assert_eq!(opacity.definitions, 1);
        assert!(opacity.source_tokens >= opacity.invocations);
        assert!(opacity.definition_tokens > 0);

        let changed_source = source.replace(
            "($item:item) => { $item }",
            "($item:item) => { if true { $item } else { $item } }",
        );
        let changed_lexed = tokens::lex(&changed_source).unwrap();
        let changed_syntax = syn::parse_file(&changed_source).unwrap();
        let changed_opacity = measure_macro_opacity(&changed_syntax, &changed_lexed);
        assert_eq!(changed_opacity.invocations, opacity.invocations);
        assert_eq!(changed_opacity.definitions, opacity.definitions);
        assert!(changed_opacity.definition_tokens > opacity.definition_tokens);
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
                        language: Language::Rust,
                        category: Category::Production,
                    },
                    DiscoveredFile {
                        path: helper_path,
                        language: Language::Rust,
                        category: Category::Production,
                    },
                    DiscoveredFile {
                        path: nested_path,
                        language: Language::Rust,
                        category: Category::Production,
                    },
                ],
                test_files: 0,
            },
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
                        language: Language::Rust,
                        category: Category::Production,
                    },
                    DiscoveredFile {
                        path: shared_path,
                        language: Language::Rust,
                        category: Category::Production,
                    },
                    DiscoveredFile {
                        path: test_only_path,
                        language: Language::Rust,
                        category: Category::Production,
                    },
                ],
                test_files: 0,
            },
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
    fn shared_external_module_cycles_preserve_production_dominance() {
        let root = std::env::temp_dir().join(format!(
            "kompass-cyclic-modules-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let lib_path = root.join("lib.rs");
        let production_path = root.join("production.rs");
        let test_root_path = root.join("test_root.rs");
        let cycle_path = root.join("cycle.rs");
        std::fs::write(
            &lib_path,
            "mod production;\n#[cfg(test)] mod test_root;\nfn entry() {}\n",
        )
        .unwrap();
        std::fs::write(&production_path, "mod cycle;\nfn production_helper() {}\n").unwrap();
        std::fs::write(&test_root_path, "mod cycle;\nfn test_helper() {}\n").unwrap();
        std::fs::write(
            &cycle_path,
            "#[path = \"production.rs\"] mod production_again;\nfn cycle_helper() {}\n",
        )
        .unwrap();

        let report = analyze(
            &root,
            Discovery {
                files: vec![
                    DiscoveredFile {
                        path: lib_path,
                        language: Language::Rust,
                        category: Category::Production,
                    },
                    DiscoveredFile {
                        path: production_path,
                        language: Language::Rust,
                        category: Category::Production,
                    },
                    DiscoveredFile {
                        path: test_root_path,
                        language: Language::Rust,
                        category: Category::Production,
                    },
                    DiscoveredFile {
                        path: cycle_path,
                        language: Language::Rust,
                        category: Category::Production,
                    },
                ],
                test_files: 0,
            },
        );

        assert_eq!(report.summary.production.functions, 3);
        assert_eq!(report.summary.test.functions, 1);
        assert_eq!(report.coverage.test_files, 1);
        assert_eq!(
            report
                .files
                .iter()
                .map(|file| (file.path.clone(), file.functions[0].category))
                .collect::<Vec<_>>(),
            vec![
                ("cycle.rs".to_owned(), Category::Production),
                ("lib.rs".to_owned(), Category::Production),
                ("production.rs".to_owned(), Category::Production),
                ("test_root.rs".to_owned(), Category::Test),
            ]
        );

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
        let collection = collect_functions(source, &syntax, &lexed, &line_analysis, false);
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
    fn line_counts_keep_literal_and_comment_blank_lines_as_content() {
        let source = concat!(
            "let raw = r#\"first\n",
            "\n",
            "third\"#;\n",
            "/* comment\n",
            "\n",
            "end */\n",
        );
        let analysis = classify_lines(source);
        let counts = analysis.counts.clone();

        assert_eq!(counts.total, 6);
        assert_eq!(counts.code, 3);
        assert_eq!(counts.comments, 3);
        assert_eq!(counts.blank, 0);
        assert_eq!(analysis.code_lines_in_range(1, 6), 3);
        assert_eq!(analysis.code_lines_in_range(2, 2), 1);
        assert_eq!(analysis.code_lines_in_range(5, 5), 0);
        assert_eq!(analysis.code_lines_in_range(8, 9), 0);
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
                    language: Language::Rust,
                    category: Category::Production,
                }],
                test_files: 0,
            },
        );

        assert!(!report.coverage.complete);
        assert_eq!(report.coverage.failed_files, 1);
        assert!(matches!(report.errors[0].kind, ErrorKind::Parse));
        std::fs::remove_file(path).unwrap();
    }
}
