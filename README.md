# Kompass

Kompass is a Rust-only command-line tool for finding the code that takes the
most thought to understand and change. It reports a transparent structural
score for every function, with the measurements behind that score, so a large
number points to a place worth reading rather than pretending to be a precise
prediction of developer time.

## Usage

Run it from a Cargo package or workspace:

```text
cargo run --release -- .
```

You can pass an explicit directory or a single Rust file. For a file named
`README-example.rs` containing this four-line fixture:

```rust
fn increment(value: i32) -> i32 {
    let next = value + 1;
    next
}
```

the normal text report is:

```text
Kompass 0.1.0 · Rust structural complexity · structural-v3
/work/my-project
1 files · 4 code lines · 20 tokens
Production · 1 functions · total burden 1.3 · average 1.3 · p95 1.3 · highest 1.3
Tests · 0 functions · total burden 0.0 · average 0.0 · p95 0.0 · highest 0.0
Repository burden · 1.3 total · 1.3 production · 0.0 tests
Macro opacity · 0 source invocations · 0 source tokens · unexpanded and excluded from score

Files by burden · top 10
    1.3  1 callables · highest 1.3 · 1.3 production · 0.0 tests · README-example.rs
Coverage complete · 1 of 1 files analyzed

Most complex production functions · sorted by score
    1.3  README-example.rs:1:1  increment
       4 lines · 4 code lines · 20 Tokens · 0 decisions · max depth 0 · 0 nesting penalty · 2 statements · 0 macro calls
       score: 1.3 = 1.0 boundary + 0.0 control decisions + 0.0 nesting + 0.0 boolean operators + 0.1 expression operations + 0.0 call sites + 0.2 explicit parameters + 0.0 match arms
```

Useful options are:

```text
kompass [PATH]                 Analyze the current directory by default
    --format text|json         Choose human or machine-readable output
    --top N                    Show N functions in text output (default 10)
    --sort score|depth|size    Order text hotspots by score, max depth, or tokens
    --model structural-v1|structural-v2|structural-v3
                               Select the versioned scoring model (default structural-v3)
    --tests                    Show test hotspots instead of production hotspots
    --all                      Show both production and test rankings
```

JSON always contains every analyzed function, regardless of `--top` and
the text selection and sorting flags, so it is safe to use in scripts. `--sort`
controls only the visible text rankings; JSON keeps its deterministic
path-and-location order. Every function has a
`category` of `production` or `test`, and the summary has separate aggregates
for both categories; no mixed production/test average is presented. Paths and
collections are sorted for stable output. A report with file-level read, lexer,
or parser errors is still printed with its successful results and exits with
status 2. An invalid input path or an output failure also exits with status 2.
The report's `model` field always identifies the selected model. For v2 and v3,
`score.value` and `score.units` are the exact integer tenths used in the
formula, while `score.display` is the presentation value; consumers should
use the integer fields for comparisons.

## Structural score

`structural-v1` is a fixed additive model with an unbounded integer score. Its
component values are shown in the text report and in JSON:

```text
score = decisions + nesting_penalty + floor(statements / 10)
```

`decisions` counts `if`, `match`, loops, `let ... else`, and each short-circuit
`&&` or `||`. A match expression contributes one decision regardless of arm
width. `nesting_penalty` adds the enclosing structural depth at each decision,
while `max_depth` records the deepest structural context. `statements` counts
executable statements in the function body; nested function bodies are scored
independently. Only `decisions`, `nesting_penalty`, and `statements` feed model
1; every other reported metric is context for readers or a future model and
does not change this score. Short-circuit operators add decisions without
adding a nesting penalty of their own.

The remaining raw metrics have fixed meanings: `branches` counts `if`,
`match`, loop, and `let ... else` constructs; `boolean_operators` counts each
`&&` and `||`; `match_arms` counts arms in every match; `loops` counts `loop`,
`for`, and `while` expressions; `returns` counts `return` expressions;
`mutations` counts assignments, compound assignments, mutable references, and
mutable pattern bindings; `parameters` counts signature inputs, including a
method receiver; `closures` counts closure expressions; `unsafe_blocks` counts
unsafe block expressions; and `macro_calls` counts expression and statement
macro invocations without expansion. These fields remain raw context and have
no effect on model 1.

The model intentionally does not use token count or line count. Tokens are a
raw size signal that helps explain a function and supports future models, while
the unbounded score preserves the difference between increasingly large
structural changes instead of compressing them into a fixed ceiling.

`structural-v2` is an experimental additive model designed for comparisons at
function, file, and repository scope. It stores integer tenths in JSON: a value
of `143` is displayed as `14.3`. The exact function formula is:

```text
function_units =
    10 * boundary
  + 10 * control_decisions
  + 10 * nesting_penalty
  +  5 * boolean_operators
  +      statements
  +  2 * call_sites
  +  2 * explicit_parameters
  +  2 * match_arms
```

Every callable has one boundary unit. `control_decisions` counts `if`,
`match`, `loop`, `for`, `while`, `let ... else`, and match guards; `&&` and
`||` are excluded from that count and are charged through
`boolean_operators`. `call_sites` counts ordinary calls and method calls.
`explicit_parameters` counts signature inputs except a method receiver, and
`match_arms` counts every arm. The v2 score is intentionally linear and
unbounded, so its units are easy to audit and compare without a hidden
normalization step.

For v2 and v3, nested named functions and closures own their bodies exclusively. The
enclosing callable still reports that a closure exists, but decisions,
statements, calls, and nesting inside the closure are scored in the closure's
own report. Each callable starts at depth zero. A file's `burden` is the sum of
its callable `score.value` fields, and the repository `summary.burden` sums
file burdens with production and test values kept separate. These aggregates
make a file with several moderate hotspots visible alongside an individual
worst function. They are exact integer units in JSON under v2 and v3.

The v2 and v3 weights are transparent, provisional hypotheses about reading
and changing Rust. The v3 `expression_operations` charge is a provisional
0.1-point weight per operation; it has no claim to be empirically optimal or
to predict developer time. A larger score is not automatically a defect. The checked-in corpus is a
structural regression and guardrail suite: it contains 13 subjective clarity
hypotheses, 8 structural invariants, 12 aggregation or opacity guardrail
probes, and 8 cost-sensitivity probes. It is not human calibration. A clarity
label records an expectation to review with people; a cost-sensitivity result
only checks that the formula responds to added arms, calls, statements,
parameters, or callable boundaries, and says nothing by itself about
readability.

The readability fixtures are compiled and analyzed from the same checked-in
source through `include_str!`, so behavior probes and score probes cannot drift
apart. The four frozen holdout cases pass alongside the declared cases. A
diagnostic sweep of 0, 0.1, 0.2, and 0.5 points per expression operation keeps
the same ranking on the declared cases, so it identifies no empirically
optimal coefficient. The examples still show tradeoffs: deduplication moves
from 5.4 to 5.5 aggregate v3 points, while naming arithmetic stays at 1.9 to
1.9 and a forwarding wrapper moves from 1.6 to 3.0; these are regression
observations, not human calibration.

Every analyzed file reports `macro_opacity.invocations` and
`macro_opacity.source_tokens` for source macro invocations whose spans can be
resolved. Definitions are excluded, invocation source is counted as written,
and expansion is never treated as a score component. The report-level value
aggregates those file measurements, and v2 and v3 text shows them next to repository
burden. A lower burden after a refactor is not a verified improvement if macro
opacity rises or coverage falls, because hidden expansion and unanalyzed files
can move work outside the measured source.

V2 and v3 text also rank files by their additive production and test burdens, and
shows each file's callable count and highest callable score so concentration is
visible without a per-file tax. Moving a callable between files leaves the
repository sum unchanged when the callable and its score are unchanged. These
file rows are an aggregation view, not a claim that a file is a semantic Rust
module; module boundaries, name resolution, and coupling burden remain future
analysis work.
Macro expansion, type resolution, generated functions, and conditional
compilation still remain outside the source-only analysis.

`structural-v3` keeps the v2 components and replaces the statement charge with
`expression_operations`:

```text
function_units =
    10 * boundary
  + 10 * control_decisions
  + 10 * nesting_penalty
  +  5 * boolean_operators
  +      expression_operations
  +  2 * call_sites
  +  2 * explicit_parameters
  +  2 * match_arms
```

`expression_operations` counts each non-short-circuit binary operation,
including compound assignment, plus unary `-`, `!`, and `*`, indexing, casts,
and ordinary assignment. `&&` and `||` keep their existing boolean-operator
charge, and calls keep their existing call-site charge. Bindings, paths,
literals, fields, borrows, grouping, and parentheses add no operation of their
own, while operands are still visited so nested operations count. Because the
metric measures expression structure rather than identifier spelling, renaming
a variable does not change the operation count or score. Introducing immutable
bindings for unchanged intermediate expressions also leaves v3 unchanged,
where v2's statement charge would increase.

The count is syntactic: equivalent code can differ when one form uses an
implicit dereference or combines work into compound assignment, because those
forms contain different operation nodes. Structural-v3 is the default;
structural-v1 and structural-v2 remain selectable, and their formulas stay
stable for existing reports.

## Token semantics

The `Tokens` value is a Rust lexical token count for the complete function
source span, from its visibility (when present) and signature through its body.
Outer attributes are outside that span. Whitespace and ordinary or
documentation comments are excluded;
identifiers, keywords, literals, punctuation, and opening and closing
delimiters each count once. A string or raw string literal is one token, and
macro invocation contents are counted as written without expanding the macro.

Multi-character punctuation is deliberately frozen as one punctuation token
per character: `->` counts as two, `::` as two, and `..=` as three. These are
Rust lexical tokens in Kompass's terminology, not LLM or model tokens. The file
total is produced by lexing the complete file once, so it is not the sum of
overlapping nested function spans.

## What gets analyzed

At a Cargo manifest root Kompass asks Cargo for workspace packages, walks each
package root, and uses Cargo's target kinds to classify integration tests and
benchmarks. A standalone directory is walked recursively in deterministic
order. Build output, Git metadata, Cargo metadata, vendored dependencies, and
Node modules are skipped. Tests are always measured with the same score model,
then classified separately when they are definite `#[cfg(test)]` module
contents, `#[test]` or known async-test functions, or Cargo test/benchmark
targets. A directory named `tests` has no special meaning in a standalone tree
without Cargo target or syntax context.

The parser is `syn`, and analysis is performed on the source that is present on
disk. Macro expansion, type resolution, generated functions, and configuration
evaluation are outside v0.1, so they are not silently presented as measured
complexity. A source file that cannot be read, lexed, or parsed appears in the
report's `errors` list and reduces the reported coverage.

## Development

```text
cargo fmt --check
cargo test
cargo test --test readability --test readability_holdout -- --nocapture
cargo clippy --all-targets --all-features -- -D warnings
```

The library is split into discovery, parsing and function collection, scoring,
lexing, report modeling, and output modules so the scoring model can evolve
without coupling it to the CLI.
