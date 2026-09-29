# Prototype status

This is an implementation experiment, not the completed M4 analyzer described by the proposal.

## Working vertical slice

- Rust 1.96+ package with a library and thin `flow` binary.
- Oxc 0.152.0 parses TS/TSX, builds semantic bindings, and constructs nonempty CFGs with the `cfg`
  feature enabled.
- Oxc resolver 11.24.3 resolves TypeScript and JavaScript imports without executing repository
  code.
- Parser-owned nodes are lowered into serializable owned tables and a small expression IR before
  the arena is dropped.
- Versioned JSON callback-factory models are strictly decoded and validated.
- `flow index` writes a content-derived snapshot with parser, symbol, function, CFG, enum, import,
  and owned-flow data.
- `flow audit` interprets a tested fragment: constants, records, finite computed selection,
  arrays, destructuring, calls, returns, closures, function-component rendering, JSX props and
  spreads, and intrinsic `onClick` registration.
- `flow query` implements the first generic query family: match a factory symbol, project selected
  creation arguments, follow one returned property as a capability, and project its invocation
  arguments. Query declarations are strict, versioned TOML and reports are JSON-serializable.
- Each configured selector alternative runs in a separate choice context. The reference audit
  preserves the registry row's key, component, callback creation, prop flow, wrapper registration,
  and invocation result.
- Evidence distinguishes key selection, value transfer, capture, prop binding, registration,
  invocation, modeled effect, and unresolved escape.

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

The `factory-query` fixture projects an array of thing types from `useHideableThing`, follows
the destructured `markHandled` callback through renamed component props, and correlates it with
the dismissal type passed at invocation. It also contains an unrendered component to verify that
`all_creations` emits an explicit unknown-reachability row and coverage gap.

## Important limitations

- This is a bounded interpreter for the first fixtures, not yet the monotone worklist solver from
  M1-M3. It has a call-depth guard but not all proposed deterministic budgets or widening rules.
- Import resolution is real, but general export/re-export symbol linkage is not implemented.
- Function and enum lookup outside the modeled import matcher is still name-based. The audit
  fixture keeps relevant declarations unambiguous and local; production claims must wait for
  resolved identities.
- Branches, loops, mutation, array methods such as `.map`, getters/proxies, exceptions/finally,
  recursion summaries, state/refs, historical renders, and custom event contracts are not analyzed
  yet.
- Query-specific unsupported operations become unresolved only when they can see a tracked
  capability in the current environment. This rule needs adversarial acceptance tests before it
  supports production absence claims.
- React event semantics are currently built into the experiment. They should be compiled from a
  versioned framework model pack before the solver boundary is considered stable.
- Additional query families, `trace` and `explain` commands, snapshot reuse/invalidation,
  line/column presentation,
  performance metrics, and worker-count determinism tests remain to be built.

## Next implementation steps

1. Link imports, exports, aliases, and re-exports to canonical declarations, then remove remaining
   name-based lookup from the interpreter.
2. Turn the owned expressions into explicit blocks/instructions and move evaluation to a monotone
   worklist with query budgets and context-keyed summaries.
3. Add mutation/cell semantics and tests for independent selections, reassigned captures, prop
   overwrite order, and opaque mutation.
4. Persist evidence indexes so `trace` and `explain` can query a chosen snapshot without reindexing.
5. Grow the semantic acceptance suite before adding concurrency or incremental invalidation.
