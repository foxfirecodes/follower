# Factory-return invocation query

`follower query` runs a declarative, creation-centered query. Its first query kind answers:

1. Where is a configured factory symbol called?
2. What values reach selected factory argument positions?
3. Where does a selected property or tuple element of the returned value flow?
4. With what values is that capability invoked?

The query is independent of names such as `useWidgetActions`, `runAction`, or `ACTION_KIND`.
Those are matcher paths, argument indexes, and output labels in TOML.

```toml
schema_version = 1
id = "widget-actions"
kind = "factory_return_invocations"
scope = "all_creations"

[factory]
project = "factory-query"
module = "src/widget.ts"
export = "useWidgetActions"

[[factory_arguments]]
index = 0
label = "widget_types"

[capability]
returned_property = ["runAction"]

[[capability.invocation_arguments]]
index = 0
label = "action_kind"
```

Run it with:

```sh
follower query --project flow.toml --query query.toml --format json
```

For a hook that returns `[selectedItem, applyAction]`, select the second element with
`returned_index = 1` instead of `returned_property`. These selectors are mutually exclusive.
Use `scan_callback_bodies = true` at the top level of the query when you want calls inside
callbacks handed to opaque libraries or component props. Those calls are candidates: the report
keeps a coverage gap because the consumer might never invoke the callback.

The synthetic tuple fixture in `tests/tuple_query.rs` uses this query shape:

```toml
schema_version = 1
id = "selected-items"
kind = "factory_return_invocations"
scope = "all_creations"
scan_callback_bodies = true

[factory]
project = "sample-app"
module = "src/useItemSelection.ts"
export = "useItemSelection"

[[factory_arguments]]
index = 0
label = "candidate_items"

[capability]
returned_index = 1

[[capability.invocation_arguments]]
index = 0
label = "action_kind"
```

Project configs can resolve repository aliases without a TypeScript compiler run:

```toml
schema_version = 1
name = "sample-app"
source_roots = [
  "src",
  "generated/itemKinds.ts",
]
source_contains_any = ["useItemSelection", "applyAction"]

[import_aliases]
"@sample/*" = "src/*"

[[callback_selector_imports]]
module = "@sample/state"
export = "selectFromStore"
callback_argument = 1
```

Alias targets are relative to the project TOML directory unless absolute. Both exact names and
single-`*` patterns are supported. Source roots can be directories or individual TS/JS files;
they are canonicalized before symbol linkage. `source_contains_any` is a fast text prefilter for
directory roots; explicitly listed files are always included. It lets a query search a
broad directory without parsing every source file. During a query, callback-bearing calls and JSX
props can pull excluded directly imported files into the analysis. Re-export barrels are not
expanded automatically. This expansion is bounded at 256 additional files and eight rounds, skips `node_modules`, and reuses
the parsed files already in that run. It does not discover reverse importers or unrelated
dependencies, so the report marks coverage incomplete whenever the prefilter is enabled.
Include enum and constant definitions as explicit file roots. Leave the prefilter unset when a
complete parse of the configured roots matters more than query latency. Parsed files are not yet
cached across separate CLI runs.

`callback_selector_imports` models imported functions that return the result of invoking one
callback argument. Each entry names the module, exported function, and zero-based argument index.
This keeps library-specific selector behavior in the project definition.

## Semantics

- Each result is centered on one dynamic creation context. Multiple finite root inputs at the same
  source callsite remain separate, so values from unrelated registry rows do not cross-pair.
- Projected values preserve strings, integral numbers, numeric enum members (name and value),
  arrays, finite array alternatives, `undefined`, and explicit unknowns. A branch-built array
  remains a set of possible arrays instead of collapsing to one merged list. String enum members
  currently project to their string values.
- `reachable` reports creations explored from configured entry points and finite input domains.
- `all_creations` additionally scans the owned IR for every resolved factory callsite. It explores
  an otherwise-unreached enclosing function once with unknown parameters. Such rows have
  `reachability = "unknown"`, an unresolved reference, and an incomplete-coverage gap; they are
  not falsely presented as reachable runtime behavior.
- An unresolved import for a call that could match the factory is an incomplete-coverage gap,
  including when no creation row can be produced.
- `candidate_invocation` means the selected capability reached at least one modeled call or React
  event path. Its row may also contain unresolved escapes for other paths.
  `absent_within_model` means no invocation or unresolved escape was found in the explored scope.
  An `unresolved` conclusion means no invocation was found and an unsupported or opaque operation
  prevents an absence claim.
- `--fail-on-unresolved` exits unsuccessfully when coverage is incomplete or any creation has
  unresolved escapes. This includes `candidate_invocation` rows with gaps and ambiguous-linkage
  reports with no creation rows. The JSON report is still printed before the unsuccessful exit.

Query report schema version 2 includes both stable source byte spans and repository-relative,
one-based line/column locations for factory calls and invocations. The text renderer uses
`path:line:column`; JSON retains both forms along with the evidence graph. Snapshot reuse is not
implemented yet.

## Current analysis fragment

The engine follows object and array destructuring, local calls and closures, records, finite computed record
selection, arrays, exact string/boolean/numeric-enum strict-equality branches, conditional expressions,
literal-array `.map`, bounded `.filter` subsets, local `.push`, JSX component props/children/spreads,
React `useMemo`, `useCallback`, `useState`, `memo`, and `forwardRef`, prop renaming, wrapper callbacks, simple
identifier reassignment, and intrinsic `onClick` handlers. Factory matching and ordinary
function/component calls, enums, and module values use canonical identities across named import
aliases, explicit renamed re-exports, local imported-then-exported aliases, `export *` chains,
namespace re-exports, and nested namespace access. Same-named declarations in unrelated modules
therefore do not change projected values or enter the call graph. Conflicting star exports are
ambiguous; explicit exports take precedence, and multiple paths to the same declaration are valid.
An ambiguous factory matcher is an error. Used ambiguous or missing import paths produce coverage
gaps. Module initializers follow entry import dependencies; unimported module creations are only
included by `all_creations`, with unknown reachability. Local enum and lexical branch bindings do
not overwrite outer bindings.

`useState` retains its initial value alongside an unknown later value. This allows candidate
calls guarded by state changes to remain visible, including calls inside effect cleanup callbacks.

An unknown `.map` receiver gets one symbolic element. It becomes a coverage gap only when the
callback, mapped result, or symbolic iteration contains a capability selected by the query.
Unknown conditions explore both branches. A partially returning branch also explores the
continuing path. Modified bindings retain joined alternatives; callback-bearing joins are
unresolved rather than disappearing after the branch. Strict equality involving an unknown value
remains unknown, and projected factory captures containing unknown or joined values carry gaps.
`for...of` is explored for zero or one iteration and switch cases independently; both leave
coverage gaps because repeated iterations and fallthrough are not modeled. `.filter` retains
possible subsets and leaves a predicate gap. Array `.push` is tracked for local bindings, with
other aliases still requiring heap modeling. Reassignment of captured bindings and opaque calls receiving callback-bearing namespaces are
conservatively unresolved. Record-property assignment is lowered, but a mutation involving a
tracked callback is conservatively unresolved because another
record alias may observe the old or new property. Precise branch correlation, heap aliasing and
live closure cells, general truthiness, non-strict comparisons, loops, and array methods other than
`.map` still require additional IR and solver work. Unsupported syntax can still hide creations or
dependencies; the current acceptance tests do not establish production absence guarantees.
