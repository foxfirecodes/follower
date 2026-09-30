# Prototype status

This is an implementation experiment, not the completed M4 analyzer described by the proposal.

## Package boundary

The prototype intentionally remains one Rust package with a library and thin binary. `frontend`,
`link`, `ir`, `analysis`/`solver`, `query`, and CLI orchestration are module boundaries while their
interfaces are still changing together. A workspace split becomes useful when at least one boundary
has an independent consumer or release/feature lifecycle—for example a reusable `follower-core`, an
Oxc-specific `follower-frontend`, and `follower-cli`. Splitting before that point would add public API and
dependency-management costs without isolating meaningful change.

## Working vertical slice

- Rust 1.96+ package with a library and thin `follower` binary.
- Oxc 0.152.0 parses TS/TSX, builds semantic bindings, and constructs nonempty CFGs with the `cfg`
  feature enabled.
- Oxc resolver 11.24.3 resolves TypeScript and JavaScript imports without executing repository
  code.
- Value-level symbol linkage follows named aliases, explicit and local re-export aliases,
  `export *` chains, namespace re-exports, and nested namespace-member access to canonical
  module/declaration identities for factories, functions/components, enums, and module values.
  Explicit exports take precedence over star exports; conflicting star paths are ambiguous, while
  diamond paths to the same declaration and cyclic barrels terminate without inventing conflicts.
- Parser-owned nodes are lowered into serializable owned tables and a small expression IR before
  the arena is dropped.
- Versioned JSON callback-factory models are strictly decoded and validated.
- `follower index` writes a content-derived snapshot with parser, symbol, function, CFG, enum, import,
  and owned-flow data.
- `follower audit` interprets a tested fragment: constants, records, finite computed selection,
  arrays, destructuring, calls, returns, closures, function-component rendering, JSX props and
  spreads, finite strict-equality branches, literal-array `.map`, simple assignment, and intrinsic
  `onClick` registration.
- `follower query` implements the first generic query family: match a factory symbol, project selected
  creation arguments, follow one returned property or tuple element as a capability, and project its invocation
  arguments. Query declarations are strict, versioned TOML and reports are JSON-serializable.
- Project-defined import aliases resolve package-style paths such as `@sample/*`; source roots can be
  individual files or directories and are canonicalized. An optional `source_contains_any` text
  prefilter limits directory parsing; callback-bearing directly imported functions/components,
  factory-argument data imports, reverse importers of exported callback-producing wrappers, and
  callers of exported hosts with unknown factory arguments are added on demand. Large importer
  files are scanned for direct imported calls and JSX uses. Expansion has file, round, expression,
  and callsite budgets. Filtered reports retain an
  incomplete-coverage marker. Configured callback selector imports support state libraries whose
  imported functions return a callback's result. Numeric enum members keep their names
  and values in query output. Local `.push`, finite array spreads, known-record `Object.values`,
  predicate-aware `.filter`, React memoization hooks,
  array destructuring, JSX children, and default/nested functions cover additional production forms.
- The query solver reuses resolved import links and module environments within each pass. Module
  environments are cleared when globals initialize, and profiling reports phase timings without
  source names. Cross-run cache and incremental solver reuse remain future work.
- Query report schema 4 includes an independent syntactic inventory of potential factory calls.
  It scans all configured sources for the factory export name and follows re-export importers to
  catch renamed uses, then labels candidate callsites as analyzed, filtered, unresolved, or
  skipped. This inventory still has syntactic and candidate-budget limits.
- Local records and arrays carry shared heap identity across aliases and helper calls. Static
  property writes and `.push` update that identity, and unknown branches join finite heap values.
- Each configured selector alternative runs in a separate choice context. The reference audit
  preserves the registry row's key, component, callback creation, prop flow, wrapper registration,
  and invocation result.
- Evidence distinguishes key selection, value transfer, mutation, branch dependency, capture, prop
  binding, registration, invocation, modeled effect, and unresolved escape.
- Module initializers follow entry-point import dependencies. Unimported modules do not produce
  reachable creations, and value dependencies resolve without relying on source-file sort order.
  Local enums and branch-local lexical bindings remain separate from outer declarations. The
  `all_creations` scan uses Oxc binding scope information to exclude shadowed factory/namespace calls.
- Unknown branch assignments retain joined alternatives and become unresolved when callbacks are
  affected. Partial returns preserve the continuing path. Unknown strict-equality operands remain
  unknown, and unknown/joined factory captures produce explicit coverage gaps. Captured-binding
  reassignment and opaque namespace escapes conservatively retain callback dependencies.
- `--fail-on-unresolved` rejects incomplete coverage and unresolved escapes, including candidate
  invocations with gaps and ambiguous-linkage reports that contain no creation rows.

The reference result is intentionally asymmetric:

| Choice | Result |
| --- | --- |
| `welcome` | The registered wrapper calls its matching callback: `candidate_invocation`. |
| `upgrade` | The registered wrapper returns its callback and React ignores the return value: `absent_within_model`. |

The coverage fixture supplies two additional checks. Passing a callback to an opaque consumer is
`unresolved`; constructing clickable JSX and discarding it does not establish rendering or event
registration.

The `prop-chain` fixture sends two independently created callbacks through the same `LevelOne` →
`LevelTwo` → `LevelThree` component chain. Each layer renames or destructures the prop. Its test
walks backward from each invocation through the evidence graph and requires the intermediate
`render_prop_binding` relations to remain attached to the matching `alpha` or `beta` choice.

The `factory-query` fixture projects an array of widget types from `useWidgetActions`, follows
the destructured `runAction` callback through renamed component props, and correlates it with
the action kind passed at invocation. Its imports pass through named aliases, multiple forms of
re-export, an `export *` barrel, and a namespace member. It also contains an unrendered component to
verify that `all_creations` emits an explicit unknown-reachability row and coverage gap.

The `query-control-flow` fixture selects correlated record arrays with a finite conditional, maps
each row to a component, takes an exact `if` branch inside that component, and verifies three
distinct type/action pairs across two root choices. The component and callback helper pass
through canonical import/re-export/namespace linkage despite unrelated declarations with the same
names. It also reassigns a local callback wrapper and checks that unrelated unknown iteration does
not degrade query coverage.

The `query-mutation` fixture aliases a callback-bearing record and then overwrites one property.
The engine preserves the candidate invocation visible through the alias and emits mutation
evidence without an aliasing gap for that local heap operation.

The `canonical_values` and `query_uncertainty` acceptance tests generate isolated source projects
covering colliding enums/constants, aliases, namespace barrels, export precedence and ambiguity,
diamond/cyclic re-exports, type-only imports, module initialization, lexical shadowing, unknown
branches, partial returns, reassigned captures, opaque namespace consumers, and CLI failure behavior.

## Important limitations

- This is a bounded interpreter for the first fixtures, not yet the monotone worklist solver from
  M1-M3. It has a call-depth guard but not all proposed deterministic budgets or widening rules.
- Branch joins preserve alternatives conservatively; they are not a precise correlated join or a
  fixed-point computation. Joined callback call targets remain unresolved. Module value cycles
  become explicit coverage gaps rather than modeling JavaScript temporal-dead-zone semantics.
- General loops, dynamic/external heap mutation and live closure cells, compound and destructuring assignment,
  array methods other than `.map`, bounded `.filter`, and local `.push`, getters/proxies,
  exceptions/finally, recursion summaries, state/refs, historical renders, and custom event
  contracts are not analyzed yet.
- Query-specific unsupported operations become unresolved only when they can see a tracked
  capability in the current environment or namespace dependencies. Initial adversarial tests cover
  the implemented boundaries, but unsupported syntax can still hide creation sites or dependencies;
  broader acceptance coverage is needed before production absence claims.
- React event semantics are currently built into the experiment. They should be compiled from a
  versioned framework model pack before the solver boundary is considered stable.
- Query reports include path/line/column presentation for creation and invocation sites. Additional
  query families, `trace` and `explain` commands, snapshot reuse/invalidation, locations on every
  evidence reference, performance metrics, and worker-count determinism tests remain to be built.

## Next implementation steps

1. Turn the owned expressions into explicit blocks/instructions and move evaluation to a monotone
   worklist with query budgets and context-keyed summaries.
2. Extend local heap cells to dynamic/external mutation, live closure cells, and points-to sets;
   extend tests for independent selections, reassigned
   captures, prop overwrite order, and opaque mutation.
3. Persist evidence indexes so `trace` and `explain` can query a chosen snapshot without reindexing.
4. Add a versioned per-file IR cache keyed by content hash, then cache import resolution and
   reuse unchanged files in query runs. Keep filtered-source gaps visible, and benchmark cold and
   warm runs on broad source roots before adding concurrency.
5. Extend the semantic acceptance suite alongside each change, especially for dependencies hidden
   by unsupported syntax, before adding concurrency or incremental invalidation.
