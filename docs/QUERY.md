# Factory-return invocation query

`flow query` runs a declarative, creation-centered query. Its first query kind answers:

1. Where is a configured factory symbol called?
2. What values reach selected factory argument positions?
3. Where does a selected property of the returned value flow?
4. With what values is that capability invoked?

The query is independent of names such as `useHideableThing`, `markHandled`, `TYPE`, or
`HIDE_KIND`. Those are matcher paths, argument indexes, and output labels in TOML.

```toml
schema_version = 1
id = "hideable-things"
kind = "factory_return_invocations"
scope = "all_creations"

[factory]
project = "factory-query"
module = "src/hideable.ts"
export = "useHideableThing"

[[factory_arguments]]
index = 0
label = "thing_types"

[capability]
returned_property = ["markHandled"]

[[capability.invocation_arguments]]
index = 0
label = "hide_kind"
```

Run it with:

```sh
flow query --project flow.toml --query query.toml --format json
```

## Semantics

- Each result is centered on one dynamic creation context. Multiple finite root inputs at the same
  source callsite remain separate, so values from unrelated registry rows do not cross-pair.
- Projected values currently preserve finite strings, arrays (recursively), `undefined`, and an
  explicit unknown with a reason. An array like `[ThingType.Banner, ThingType.Modal]` therefore
  remains an array of two strings in JSON.
- `reachable` reports creations explored from configured entry points and finite input domains.
- `all_creations` additionally scans the owned IR for every resolved factory callsite. It explores
  an otherwise-unreached enclosing function once with unknown parameters. Such rows have
  `reachability = "unknown"`, an unresolved reference, and an incomplete-coverage gap; they are
  not falsely presented as reachable runtime behavior.
- `candidate_invocation` means the selected capability reached at least one modeled call or React
  event path. Its row may also contain unresolved escapes for other paths.
  `absent_within_model` means no invocation or unresolved escape was found in the explored scope.
  An `unresolved` conclusion means no invocation was found and an unsupported or opaque operation
  prevents an absence claim.
- `--fail-on-unresolved` exits unsuccessfully when a creation's conclusion is `unresolved`.

Query report schema version 2 includes both stable source byte spans and repository-relative,
one-based line/column locations for factory calls and invocations. The text renderer uses
`path:line:column`; JSON retains both forms along with the evidence graph. Snapshot reuse is not
implemented yet.

## Current analysis fragment

The engine follows destructuring, local calls and closures, records, finite computed record
selection, arrays, exact string/boolean strict-equality branches, conditional expressions,
literal-array `.map`, JSX component props/spreads, prop renaming, wrapper callbacks, simple
identifier reassignment, and intrinsic `onClick` handlers. Factory matching and ordinary
function/component calls use canonical identities across named import aliases, explicit renamed
re-exports, local imported-then-exported aliases, `export *` chains, and direct namespace member
calls. Same-named declarations in unrelated modules therefore do not enter the call graph.

An unknown `.map` receiver gets one symbolic element. It becomes a coverage gap only when the
callback, mapped result, or symbolic iteration contains a capability selected by the query.
Unknown conditions explore both branches; a branch that returns on only one side likewise records
a gap only when that partial return contains a selected capability. Record-property assignment is
lowered, but a mutation involving a tracked callback is conservatively unresolved because another
record alias may observe the old or new property. Namespace re-exports (`export * as ns`), precise
heap aliasing, general truthiness, non-strict comparisons, loops, and array methods other than
`.map` still require additional IR and solver work.
