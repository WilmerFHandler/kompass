# JavaScript and TypeScript acceptance corpus

These fixtures are small source-only probes for the JavaScript/TypeScript
frontend. The acceptance tests invoke the Kompass CLI and inspect the same
JSON envelope used by the Rust and Python frontends; they are intentionally
marked `#[ignore]` until that frontend is integrated.

The corpus specifies these stable behaviors:

- `.js` and `.jsx` are JavaScript; `.ts` and `.tsx` are TypeScript.
- React components, custom hooks, ordinary arrows, event handlers, effects,
  and iterator callbacks are separate callable units. A callback owns its
  body exactly once, while its surrounding lexical control-flow depth is
  retained.
- JSX expression containers contribute their executable expressions. JSX
  markup, type annotations, interfaces, and type assertions do not invent
  runtime score signals.
- Optional chaining, nullish coalescing, ternaries, `switch`, and `catch`
  remain visible structural signals under `structural-v4`.
- Anonymous/default exports, class methods, class-field initializers, and
  module-level executable initializers remain visible units.
- Test naming conventions classify test files separately. Dependency trees,
  generated output, coverage, and Expo state are outside the analyzed scope.
- A malformed source file produces a partial report with an error and
  incomplete coverage while valid sibling files remain analyzable.
- Snapshot identities stay unique for same-named units, duplicate evidence
  remains in the `evidence-v1` envelope, and type erasure leaves runtime
  metrics and scores unchanged.
