//! Conservative, source-only evidence for refactoring comparisons.
//!
//! Evidence is deliberately kept outside the structural score.  The score is
//! a stable measurement of syntax shape; this module adds relationships that
//! can be useful during review when the source gives us enough information to
//! make a bounded claim.  It never performs name resolution, type inference,
//! macro expansion, or control-flow analysis.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::{Category, FunctionKind, FunctionReport, Language, Location};

/// Stable identity of the serialized evidence contract.
pub const EVIDENCE_CONTRACT: &str = "evidence-v1";

/// Maximum number of distinct callable units included in a call region.
pub const MAX_REGION_UNITS: usize = 128;

/// Maximum acyclic depth explored for a call region.
pub const MAX_REGION_DEPTH: usize = 32;

/// Maximum number of duplicate groups a text renderer should show by default.
/// JSON retains every group; the omitted count is calculated by the renderer.
pub const DEFAULT_DUPLICATE_RENDER_LIMIT: usize = 20;

/// All conservative evidence produced for one report.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Evidence {
    pub contract: String,
    pub call_graph: CallGraphEvidence,
    pub duplicates: DuplicateEvidence,
}

impl Evidence {
    pub fn current() -> Self {
        Self {
            contract: EVIDENCE_CONTRACT.to_owned(),
            call_graph: CallGraphEvidence::default(),
            duplicates: DuplicateEvidence::default(),
        }
    }
}

/// Local calls and bounded reachability regions.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CallGraphEvidence {
    pub coverage: CallCoverage,
    pub edges: Vec<CallEdge>,
    pub regions: Vec<CallRegion>,
}

/// Counts for every syntactic call site considered by the frontends.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CallCoverage {
    pub call_sites: usize,
    pub resolved: usize,
    pub unresolved: usize,
    pub ambiguous: usize,
    pub resolved_ratio: f64,
}

/// One source-only call relationship.  `callee` is present only for a unique
/// direct local function match.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallEdge {
    pub caller: CallableRef,
    pub callee: Option<CallableRef>,
    pub callee_name: String,
    pub resolution: CallResolution,
    pub reason: CallResolutionReason,
    pub location: Location,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallResolution {
    Resolved,
    Unresolved,
    Ambiguous,
}

/// Why a call could not be promoted to a local edge, or why a direct call was
/// resolved.  The frontend marks syntactic cases; the common builder marks
/// duplicate or missing definitions after it sees the full file.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallResolutionReason {
    DirectLocal,
    Method,
    Import,
    Parameter,
    Assignment,
    Alias,
    Dynamic,
    Qualified,
    DuplicateDefinition,
    Unknown,
}

/// Stable source reference to one reported callable.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct CallableRef {
    pub path: String,
    pub language: Language,
    pub category: Category,
    pub name: String,
    pub kind: FunctionKind,
    pub location: Location,
}

/// A bounded region rooted at one callable.  `reachable` contains each unit
/// once, and `burden` sums each unit's existing score once as well.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallRegion {
    pub root: CallableRef,
    pub reachable: Vec<CallableRef>,
    pub unique_units: usize,
    pub burden: usize,
    pub max_depth: usize,
    pub recursive: bool,
    pub truncated: bool,
}

/// Exact duplicate statement sequences.  All groups are retained in JSON;
/// text output may render a bounded prefix and report how many it omitted.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DuplicateEvidence {
    pub groups: Vec<DuplicateGroup>,
    pub omitted: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub language: Language,
    pub category: Category,
    pub statement_count: usize,
    pub tokens: usize,
    pub scored_signals: usize,
    pub fingerprint: String,
    pub occurrences: Vec<DuplicateOccurrence>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DuplicateOccurrence {
    pub callable: CallableRef,
    pub start: Location,
    pub end: Location,
}

/// Frontend-owned, source-local evidence before paths and callable metadata
/// are attached by the report analyzer.
#[derive(Clone, Debug, Default)]
pub struct FrontendEvidence {
    pub calls: Vec<FrontendCall>,
    pub statements: Vec<FrontendStatement>,
}

/// A syntactic call observed inside a callable body.  A direct name is left
/// as `DirectCandidate`; the common builder decides whether its definition is
/// unique.  Other variants are intentionally never resolved.
#[derive(Clone, Debug)]
pub struct FrontendCall {
    pub caller: LocalCallable,
    pub name: String,
    pub reason: CallResolutionReason,
    pub location: Location,
}

/// One statement in one lexical statement list. `sequence` identifies the
/// list, preventing unrelated nested blocks from being treated as adjacent.
#[derive(Clone, Debug)]
pub struct FrontendStatement {
    pub caller: LocalCallable,
    pub sequence: usize,
    pub ordinal: usize,
    pub location: Location,
    pub tokens: Vec<String>,
    pub scored_signals: usize,
}

/// Frontends use this identity to attach calls and statements to reports.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LocalCallable {
    pub name: String,
    pub kind: FunctionKind,
    pub category: Category,
    pub location: Location,
}

/// Input for the common evidence builder.
#[derive(Clone, Debug)]
pub struct EvidenceFile {
    pub path: String,
    pub language: Language,
    pub functions: Vec<FunctionReport>,
    pub frontend: FrontendEvidence,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CallableKey {
    path: String,
    language: Language,
    category: Category,
    name: String,
    kind: FunctionKind,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
}

#[derive(Clone, Debug)]
struct CallableNode {
    key: CallableKey,
    reference: CallableRef,
    score: usize,
}

#[derive(Clone, Debug)]
struct StatementStream {
    language: Language,
    category: Category,
    owner: CallableKey,
    statements: Vec<FrontendStatement>,
}

/// Build all evidence for analyzed files.  The builder is deterministic and
/// intentionally bounded only for call-region traversal; duplicate analysis
/// retains every qualifying group for machine-readable review.
pub fn build(files: &[EvidenceFile]) -> Evidence {
    let nodes = collect_nodes(files);
    let (call_graph, resolved_edges) = build_call_graph(files, &nodes);
    let duplicates = build_duplicates(files, &nodes);
    let _ = resolved_edges;
    Evidence {
        contract: EVIDENCE_CONTRACT.to_owned(),
        call_graph,
        duplicates,
    }
}

fn collect_nodes(files: &[EvidenceFile]) -> BTreeMap<CallableKey, CallableNode> {
    let mut nodes = BTreeMap::new();
    for file in files {
        for function in &file.functions {
            let key = callable_key(&file.path, file.language, function);
            let reference = CallableRef {
                path: file.path.clone(),
                language: file.language,
                category: function.category,
                name: function.name.clone(),
                kind: function.kind.clone(),
                location: function.location.clone(),
            };
            nodes.insert(
                key.clone(),
                CallableNode {
                    key,
                    reference,
                    score: function.score.value,
                },
            );
        }
    }
    nodes
}

fn callable_key(path: &str, language: Language, function: &FunctionReport) -> CallableKey {
    CallableKey {
        path: path.to_owned(),
        language,
        category: function.category,
        name: function.name.clone(),
        kind: function.kind.clone(),
        start_line: function.location.start.line,
        start_column: function.location.start.column,
        end_line: function.location.end.line,
        end_column: function.location.end.column,
    }
}

fn local_key(path: &str, language: Language, local: &LocalCallable) -> CallableKey {
    CallableKey {
        path: path.to_owned(),
        language,
        category: local.category,
        name: local.name.clone(),
        kind: local.kind.clone(),
        start_line: local.location.start.line,
        start_column: local.location.start.column,
        end_line: local.location.end.line,
        end_column: local.location.end.column,
    }
}

fn build_call_graph(
    files: &[EvidenceFile],
    nodes: &BTreeMap<CallableKey, CallableNode>,
) -> (CallGraphEvidence, BTreeMap<CallableKey, Vec<CallableKey>>) {
    let mut by_name = BTreeMap::<(String, Language, String), Vec<CallableKey>>::new();
    for node in nodes.values() {
        by_name
            .entry((
                node.key.path.clone(),
                node.key.language,
                leaf_name(&node.reference.name),
            ))
            .or_default()
            .push(node.key.clone());
    }
    for candidates in by_name.values_mut() {
        candidates.sort();
    }

    let mut edges = Vec::new();
    let mut adjacency = BTreeMap::<CallableKey, Vec<CallableKey>>::new();
    for file in files {
        for call in &file.frontend.calls {
            let caller_key = local_key(&file.path, file.language, &call.caller);
            let Some(caller) = nodes.get(&caller_key) else {
                continue;
            };
            let (resolution, reason, callee) = if call.reason != CallResolutionReason::DirectLocal {
                (CallResolution::Unresolved, call.reason, None)
            } else {
                let candidates = by_name
                    .get(&(file.path.clone(), file.language, call.name.clone()))
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|key| {
                        nodes
                            .get(key)
                            .is_some_and(|node| direct_target_kind(&node.reference.kind))
                    })
                    .collect::<Vec<_>>();
                match candidates.as_slice() {
                    [only] => (
                        CallResolution::Resolved,
                        CallResolutionReason::DirectLocal,
                        nodes.get(only).map(|node| node.reference.clone()),
                    ),
                    [] => (
                        CallResolution::Unresolved,
                        CallResolutionReason::Unknown,
                        None,
                    ),
                    _ => (
                        CallResolution::Ambiguous,
                        CallResolutionReason::DuplicateDefinition,
                        None,
                    ),
                }
            };
            let callee_key = callee.as_ref().and_then(|reference| {
                nodes
                    .values()
                    .find(|node| node.reference == *reference)
                    .map(|node| node.key.clone())
            });
            if let Some(callee_key) = callee_key.clone() {
                adjacency
                    .entry(caller.key.clone())
                    .or_default()
                    .push(callee_key);
            }
            edges.push(CallEdge {
                caller: caller.reference.clone(),
                callee,
                callee_name: call.name.clone(),
                resolution,
                reason,
                location: call.location.clone(),
            });
        }
    }

    edges.sort_by(|left, right| {
        left.caller
            .cmp(&right.caller)
            .then_with(|| left.location.cmp(&right.location))
            .then_with(|| left.callee_name.cmp(&right.callee_name))
    });
    for targets in adjacency.values_mut() {
        targets.sort();
        targets.dedup();
    }

    let mut coverage = CallCoverage {
        call_sites: edges.len(),
        ..CallCoverage::default()
    };
    for edge in &edges {
        match edge.resolution {
            CallResolution::Resolved => coverage.resolved += 1,
            CallResolution::Unresolved => coverage.unresolved += 1,
            CallResolution::Ambiguous => coverage.ambiguous += 1,
        }
    }
    coverage.resolved_ratio = if coverage.call_sites == 0 {
        0.0
    } else {
        coverage.resolved as f64 / coverage.call_sites as f64
    };

    let regions = build_regions(nodes, &adjacency);
    (
        CallGraphEvidence {
            coverage,
            edges,
            regions,
        },
        adjacency,
    )
}

fn direct_target_kind(kind: &FunctionKind) -> bool {
    matches!(kind, FunctionKind::Function | FunctionKind::NestedFunction)
}

fn leaf_name(name: &str) -> String {
    name.rsplit("::").next().unwrap_or(name).to_owned()
}

fn build_regions(
    nodes: &BTreeMap<CallableKey, CallableNode>,
    adjacency: &BTreeMap<CallableKey, Vec<CallableKey>>,
) -> Vec<CallRegion> {
    let mut regions = Vec::new();
    for root in nodes.values() {
        let mut reachable = BTreeSet::new();
        reachable.insert(root.key.clone());
        let mut stack = vec![(root.key.clone(), 0usize, vec![root.key.clone()])];
        let mut max_depth = 0;
        let mut recursive = false;
        let mut truncated = false;
        while let Some((current, depth, path)) = stack.pop() {
            max_depth = max_depth.max(depth);
            for target in adjacency.get(&current).into_iter().flatten() {
                if path.iter().any(|ancestor| ancestor == target) {
                    recursive = true;
                    continue;
                }
                if depth >= MAX_REGION_DEPTH {
                    truncated = true;
                    continue;
                }
                if reachable.len() >= MAX_REGION_UNITS && !reachable.contains(target) {
                    truncated = true;
                    continue;
                }
                let inserted = reachable.insert(target.clone());
                if inserted {
                    let mut next_path = path.clone();
                    next_path.push(target.clone());
                    stack.push((target.clone(), depth + 1, next_path));
                }
            }
        }
        let mut reachable_nodes = reachable
            .iter()
            .filter_map(|key| nodes.get(key))
            .collect::<Vec<_>>();
        reachable_nodes.sort_by(|left, right| left.reference.cmp(&right.reference));
        let burden = reachable_nodes.iter().map(|node| node.score).sum::<usize>();
        let reachable = reachable_nodes
            .into_iter()
            .map(|node| node.reference.clone())
            .collect::<Vec<_>>();
        regions.push(CallRegion {
            root: root.reference.clone(),
            unique_units: reachable.len(),
            reachable,
            burden,
            max_depth,
            recursive,
            truncated,
        });
    }
    regions.sort_by(|left, right| left.root.cmp(&right.root));
    regions
}

fn build_duplicates(
    files: &[EvidenceFile],
    nodes: &BTreeMap<CallableKey, CallableNode>,
) -> DuplicateEvidence {
    let mut streams =
        BTreeMap::<(String, Language, Category, LocalCallable, usize), StatementStream>::new();
    for file in files {
        for statement in &file.frontend.statements {
            let owner = local_key(&file.path, file.language, &statement.caller);
            if !nodes.contains_key(&owner) {
                continue;
            }
            let key = (
                file.path.clone(),
                file.language,
                statement.caller.category,
                statement.caller.clone(),
                statement.sequence,
            );
            streams
                .entry(key)
                .or_insert_with(|| StatementStream {
                    language: file.language,
                    category: statement.caller.category,
                    owner,
                    statements: Vec::new(),
                })
                .statements
                .push(statement.clone());
        }
    }
    let mut streams = streams.into_values().collect::<Vec<_>>();
    for stream in &mut streams {
        stream.statements.sort_by_key(|statement| statement.ordinal);
    }

    let mut candidates = BTreeMap::<SequenceKey, BTreeSet<OccurrenceKey>>::new();
    for left_index in 0..streams.len() {
        for right_index in left_index..streams.len() {
            if streams[left_index].language != streams[right_index].language
                || streams[left_index].category != streams[right_index].category
            {
                continue;
            }
            let left_len = streams[left_index].statements.len();
            let right_len = streams[right_index].statements.len();
            for left_start in 0..left_len {
                let right_start_min = if left_index == right_index {
                    left_start.saturating_add(1)
                } else {
                    0
                };
                for right_start in right_start_min..right_len {
                    let length = common_prefix(
                        &streams[left_index].statements[left_start..],
                        &streams[right_index].statements[right_start..],
                    );
                    if length < 3 {
                        continue;
                    }
                    let sequence = &streams[left_index].statements[left_start..left_start + length];
                    let tokens = sequence
                        .iter()
                        .map(|statement| statement.tokens.len())
                        .sum::<usize>();
                    let scored_signals = sequence
                        .iter()
                        .map(|statement| statement.scored_signals)
                        .sum::<usize>();
                    if tokens < 40 || scored_signals < 2 {
                        continue;
                    }
                    let key = SequenceKey {
                        language: streams[left_index].language,
                        category: streams[left_index].category,
                        statements: sequence
                            .iter()
                            .map(|statement| statement.tokens.clone())
                            .collect(),
                    };
                    candidates.entry(key).or_default().extend([
                        OccurrenceKey {
                            stream: left_index,
                            start: left_start,
                        },
                        OccurrenceKey {
                            stream: right_index,
                            start: right_start,
                        },
                    ]);
                }
            }
        }
    }

    let mut candidate_keys = candidates.keys().cloned().collect::<Vec<_>>();
    candidate_keys.sort_by(|left, right| {
        right
            .statements
            .len()
            .cmp(&left.statements.len())
            .then_with(|| right.token_count().cmp(&left.token_count()))
            .then_with(|| left.cmp(right))
    });
    let mut accepted = Vec::<SequenceKey>::new();
    for candidate in candidate_keys {
        if accepted
            .iter()
            .any(|larger| larger.contains_strict(&candidate))
        {
            continue;
        }
        accepted.push(candidate);
    }

    let mut groups = Vec::new();
    for key in accepted {
        let mut occurrences = BTreeSet::new();
        for (index, stream) in streams.iter().enumerate() {
            if stream.language != key.language || stream.category != key.category {
                continue;
            }
            for start in 0..stream.statements.len() {
                if sequence_matches(&stream.statements[start..], &key.statements) {
                    occurrences.insert(OccurrenceKey {
                        stream: index,
                        start,
                    });
                }
            }
        }
        if occurrences.len() < 2 {
            continue;
        }
        let first = &streams[occurrences.iter().next().map_or(0, |item| item.stream)];
        let sequence = &first.statements[occurrences.iter().next().map_or(0, |item| item.start)..];
        let statement_count = key.statements.len();
        let tokens = key.token_count();
        let scored_signals = sequence
            .iter()
            .take(statement_count)
            .map(|statement| statement.scored_signals)
            .sum::<usize>();
        let mut rendered_occurrences = occurrences
            .into_iter()
            .filter_map(|occurrence| {
                let stream = streams.get(occurrence.stream)?;
                let first = stream.statements.get(occurrence.start)?;
                let last = stream
                    .statements
                    .get(occurrence.start + statement_count - 1)?;
                let owner = nodes.get(&stream.owner)?;
                Some(DuplicateOccurrence {
                    callable: owner.reference.clone(),
                    start: first.location.clone(),
                    end: last.location.clone(),
                })
            })
            .collect::<Vec<_>>();
        rendered_occurrences.sort_by(|left, right| {
            left.callable
                .cmp(&right.callable)
                .then_with(|| left.start.cmp(&right.start))
        });
        groups.push(DuplicateGroup {
            language: key.language,
            category: key.category,
            statement_count,
            tokens,
            scored_signals,
            fingerprint: key.fingerprint(),
            occurrences: rendered_occurrences,
        });
    }
    groups.sort_by(|left, right| {
        right
            .tokens
            .cmp(&left.tokens)
            .then_with(|| left.language.cmp(&right.language))
            .then_with(|| left.category.cmp(&right.category))
            .then_with(|| left.fingerprint.cmp(&right.fingerprint))
    });
    DuplicateEvidence { groups, omitted: 0 }
}

fn common_prefix(left: &[FrontendStatement], right: &[FrontendStatement]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left.tokens == right.tokens)
        .count()
}

fn sequence_matches(statements: &[FrontendStatement], sequence: &[Vec<String>]) -> bool {
    statements.len() >= sequence.len()
        && statements
            .iter()
            .zip(sequence)
            .all(|(statement, tokens)| statement.tokens == *tokens)
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SequenceKey {
    language: Language,
    category: Category,
    statements: Vec<Vec<String>>,
}

impl SequenceKey {
    fn token_count(&self) -> usize {
        self.statements.iter().map(Vec::len).sum::<usize>()
    }

    fn contains_strict(&self, candidate: &Self) -> bool {
        self.language == candidate.language
            && self.category == candidate.category
            && self.statements.len() > candidate.statements.len()
            && self
                .statements
                .windows(candidate.statements.len())
                .any(|window| window == candidate.statements.as_slice())
    }

    fn fingerprint(&self) -> String {
        self.statements
            .iter()
            .map(|statement| statement.join(" "))
            .collect::<Vec<_>>()
            .join(" ␞ ")
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct OccurrenceKey {
    stream: usize,
    start: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(name: &str, category: Category, score: usize, line: usize) -> FunctionReport {
        FunctionReport {
            snapshot_id: format!("snapshot-{name}-{line}"),
            declaration_fingerprint: format!("declaration-{name}"),
            body_fingerprint: format!("body-{name}-{line}"),
            name: name.to_owned(),
            kind: FunctionKind::Function,
            category,
            location: Location {
                start: crate::model::Position { line, column: 1 },
                end: crate::model::Position {
                    line: line + 4,
                    column: 2,
                },
            },
            lines: 5,
            tokens: 1,
            metrics: Default::default(),
            score: crate::model::Score {
                value: score,
                units: score,
                ..Default::default()
            },
        }
    }

    fn local(function: &FunctionReport) -> LocalCallable {
        LocalCallable {
            name: function.name.clone(),
            kind: function.kind.clone(),
            category: function.category,
            location: function.location.clone(),
        }
    }

    #[test]
    fn only_unique_direct_names_resolve() {
        let caller = function("caller", Category::Production, 10, 1);
        let target = function("target", Category::Production, 20, 10);
        let frontend = FrontendEvidence {
            calls: vec![
                FrontendCall {
                    caller: local(&caller),
                    name: "target".to_owned(),
                    reason: CallResolutionReason::DirectLocal,
                    location: Location::default(),
                },
                FrontendCall {
                    caller: local(&caller),
                    name: "method".to_owned(),
                    reason: CallResolutionReason::Method,
                    location: Location::default(),
                },
            ],
            ..Default::default()
        };
        let evidence = build(&[EvidenceFile {
            path: "main.rs".to_owned(),
            language: Language::Rust,
            functions: vec![caller, target],
            frontend,
        }]);
        assert_eq!(evidence.call_graph.coverage.resolved, 1);
        assert_eq!(evidence.call_graph.coverage.unresolved, 1);
        assert!(
            evidence
                .call_graph
                .edges
                .iter()
                .any(|edge| edge.resolution == CallResolution::Resolved)
        );
        assert!(
            evidence
                .call_graph
                .edges
                .iter()
                .any(|edge| edge.reason == CallResolutionReason::Method)
        );
    }

    #[test]
    fn regions_charge_reachable_units_once_and_mark_recursion() {
        let a = function("a", Category::Production, 10, 1);
        let b = function("b", Category::Production, 20, 10);
        let c = function("c", Category::Production, 30, 20);
        let calls = |caller: &FunctionReport, name: &str| FrontendCall {
            caller: local(caller),
            name: name.to_owned(),
            reason: CallResolutionReason::DirectLocal,
            location: Location::default(),
        };
        let evidence = build(&[EvidenceFile {
            path: "main.rs".to_owned(),
            language: Language::Rust,
            functions: vec![a.clone(), b.clone(), c.clone()],
            frontend: FrontendEvidence {
                calls: vec![calls(&a, "b"), calls(&b, "c"), calls(&c, "a")],
                statements: Vec::new(),
            },
        }]);
        let region = evidence
            .call_graph
            .regions
            .iter()
            .find(|region| region.root.name == "a")
            .unwrap();
        assert_eq!(region.unique_units, 3);
        assert_eq!(region.burden, 60);
        assert!(region.recursive);
    }

    #[test]
    fn duplicate_sequences_require_thresholds_and_suppress_contained_groups() {
        let left = function("left", Category::Production, 10, 1);
        let right = function("right", Category::Production, 10, 20);
        let statement =
            |owner: &FunctionReport, sequence: usize, ordinal: usize| FrontendStatement {
                caller: local(owner),
                sequence,
                ordinal,
                location: Location {
                    start: crate::model::Position {
                        line: ordinal + 1,
                        column: 1,
                    },
                    end: crate::model::Position {
                        line: ordinal + 1,
                        column: 4,
                    },
                },
                tokens: vec!["same".to_owned(); 15],
                scored_signals: 1,
            };
        let mut statements = Vec::new();
        for owner in [&left, &right] {
            for ordinal in 0..3 {
                statements.push(statement(owner, 0, ordinal));
            }
        }
        let evidence = build(&[EvidenceFile {
            path: "main.rs".to_owned(),
            language: Language::Rust,
            functions: vec![left, right],
            frontend: FrontendEvidence {
                calls: Vec::new(),
                statements,
            },
        }]);
        assert_eq!(evidence.duplicates.groups.len(), 1);
        assert_eq!(evidence.duplicates.groups[0].statement_count, 3);
        assert_eq!(evidence.duplicates.groups[0].tokens, 45);
    }
}
