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

Add `--html-report` to write an interactive, self-contained report to a new owner-readable file
in `/tmp`. The command prints its path to stderr and leaves JSON or text stdout unchanged. Set
`[report] html = true` in the query TOML to generate it on every run. The report is also written
before a `--fail-on-unresolved` failure. It contains query values and paths, so treat the file as
private and remove it when finished.

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
props can pull excluded directly imported files into the analysis. Unknown factory arguments can
pull in the imported constants they depend on. A lightweight import scan also finds files that
import exported wrappers returning the tracked callback or exported functions/components whose
factory arguments are still unknown. It follows re-export barrels and evaluates direct imported
calls and JSX uses in large importer files. The analysis expansion is bounded at 256 additional
files and eight rounds, skips `node_modules`, and reuses parsed files within the run; the backward
use walk below has its own 256-file budget. Importer
functions below 20,000 bytes are evaluated with a 5,000-expression budget; larger files use direct
callsites with unknown surrounding locals and a 64-callsite budget per function. These limits and
the text filter leave coverage incomplete.
Hypothetical renders from an unreached factory host have a separate 5,000-step budget covering
expression evaluation and component visits. Exhaustion stops that render, marks carried callback
values unresolved, and appears as a coverage gap; other factory callsites are still examined.
Include enum and constant definitions as explicit file roots. Leave the prefilter unset when a
complete parse of the configured roots matters more than query latency. A filtered query reads
each configured source once to build both the initial index and the import catalog. Parsed files
are not yet cached across separate CLI runs. One module resolver, with its filesystem and
canonical-path caches, serves the whole query run, so files must not change while it runs. During
each solver pass, module environments and resolved import links are reused; environments are
invalidated when module globals initialize. Symbol linkage and the solver use per-snapshot file
and import-resolution indexes instead of repeatedly scanning the snapshot, and each local
binding's linkage walk is computed once per solver pass. Solver values share closures, records, arrays, unions, and JSX elements and copy them only on
write, so branch joins and calls do not deep-copy captured environments. While an unknown branch
runs, heap writes record the value they replaced; the branch is rewound from that record, and the
join visits only the heap entries either side wrote rather than the whole heap. Set
`FOLLOWER_PROFILE_QUERY=1` to print generic phase timings and cache counts to stderr; use a release
build when comparing timings.

Configured `[[entries]]` modules are indexed even if the text prefilter does not match them. This
does not index every source file: the initial parse still uses the filter, then import expansion
adds files needed for modeled flow. After that expansion settles, queries with entries use the
lightweight import graph to find candidate paths from factory callsites toward entries. A backward
use walk follows the containing function or module binding through local references, exports,
re-exports, and imported references. It parses candidate importers on demand, prioritizing paths
nearer the entry. The walk indexes resolved import targets as files are added, so repeated symbol
checks do not rescan the growing resolution list. Root reachability is then evaluated forward
through modeled calls and JSX in the expanded snapshot. The walk runs once discovery stops, whether
it settled or reached its budget. For filtered `all_creations` queries, the roots run at the same
point; a discovery stop remains a coverage gap.
A root phase then parses only what those root paths need: imported values they read, unknown
components whose files are on the import corridor toward factory hosts, and unknown components,
including member tags such as `<Context.Provider>`, that receive children, `render`, or
`component` props. It has its own budget of 512 files and eight rounds, reported as a gap when
reached. A round that requests more files than remain takes them in the order root paths met
them, nearest the entry first.

Requests follow linkage rather than only the imported file: when an imported binding does not
link, the files requested are the unparsed modules its import and re-export chain reached. A
barrel's `export *` targets are often mostly unparsed. Such a target is ruled out without parsing
when its text cannot export the name: an ES module exports only names it spells out, so a module
that never mentions the name and has no forwarding syntax (`export *`, TS `export =`, identifier
escapes, or CommonJS `exports` used other than as `exports.name` or
`Object.defineProperty(exports, "name", ...)`) cannot provide it. Non-script files such as
stylesheets may export anything. Only the remaining candidates are requested, and the binding links
once they are parsed. A type-only import of a specifier does not hide a value re-export of the same
specifier. A parsed wrapper does not hide paths: JSX it drops after
reaching something the model cannot follow is still explored as possible, as described below. An
import or symbol reference alone does not prove root reachability or
runtime rendering. Unsupported dynamic loaders and registry wiring can still leave paths unknown;
budget limits are reported as coverage gaps. Each expansion round currently rebuilds the solver and
re-evaluates previously discovered reverse importer seeds. Since query report schema version 7, the
report records the seed's function or module binding, source location, matched import names, and evaluation mode
in each creation's optional `reverse_importer` field. The HTML detail and creation list show it.
`unresolved_count` remains available even when `include_unresolved_escapes = false` hides the
individual references; strict exit checks use the count.

`callback_selector_imports` models imported functions that return the result of invoking one
callback argument. Each entry names the module, exported function, and zero-based argument index.
This keeps library-specific selector behavior in the project definition.

Reachability can use explicit contracts for imported routing and code-splitting helpers:

```toml
[[component_consumers]]
module = "@sample/router"
export = "Route"
render_props = ["render"]
component_props = ["component"]
# For a focused run, optionally explore only named callbacks. Skips are coverage gaps.
render_callback_names = ["showMain"]

[[component_consumers]]
module = "@sample/router"
export = "Switch"
forward_children = true

[[component_consumers]]
module = "@sample/overlay"
export = "Overlay"
invoke_children = true

[[component_wrappers]]
module = "@sample/auth"
export = "withAccess"
component_argument = 0

[[lazy_component_factories]]
module = "@sample/lazy"
export = "loadComponent"
promise_property = "load"
```

A consumer contract explores the named render callback, component prop, children, or a
function-valued child as a possible render. It applies to a JSX tag imported from its module and
export, and to any tag whose import passes through that export on the way to its definition, so
a contract on a package export also covers barrels that re-export it under the same name. A wrapper contract says the returned component may
render the component at the given argument index. A lazy factory contract recognizes a callback
returning a literal `import()` and links its default export. These contracts describe possible
paths, not guaranteed
route matches, authorization, loading, or runtime rendering. Unconfigured imports and dynamic
import expressions remain unknown. The backward walk still parses only candidate importer paths;
class `render()` methods and direct `this.method()` calls can supply use edges without indexing
every file.
Object-rest props, JSX fragments, and configured member tags can forward children through the
same slice. As in React, a single JSX child is passed as `children` itself and several children as
an array, so a component can call a function child. JSX text follows the React whitespace rule:
indentation-only text is dropped, and other text is a string child. Contexts created by React `createContext` need no contract: their `Provider` renders
its children and their `Consumer` calls its function child. A configured render callback name filter is intended
for focused traces; each skipped callback is reported as a coverage gap. Repeated renders at one
source site are bounded to 16 visits per component and callback identity and per set of JSX it
receives, with a coverage gap if the bound is reached; a wrapper used in many places therefore
still renders the children given at each use. Filtered `all_creations` queries postpone entry execution until import and
backward-use expansion has settled, avoiding repeated full entry walks during early rounds.

## Semantics

- Each result is centered on one dynamic creation context. Multiple finite root inputs at the same
  source callsite remain separate, so values from unrelated registry rows do not cross-pair.
- Projected values preserve strings, integral numbers, numeric enum members (name and value),
  arrays, finite array alternatives, `null`, `undefined`, and explicit unknowns. A branch-built array
  remains a set of possible arrays instead of collapsing to one merged list. String enum members
  currently project to their string values.
- `reachable` reports creations explored from configured entry points and finite input domains.
- A path from an entry can pass through a component whose behavior is not modeled, such as one
  imported from a file the text prefilter skipped or from an external package. The explorer then
  assumes the component renders its children, JSX-valued props, component props, and render
  functions that return JSX. Creations found below it have `reachability = "possible"`, an
  unresolved reference at each assumed component, and a directly linked
  `render_assumed_through_unmodeled_component` gap. Invocation paths mark the step as
  `assumed_render`. Callbacks passed to the component are not called by this assumption. A
  possible row is never upgraded to reachable; modeling or parsing the component can remove the
  assumption. Function children are called like render functions. A call to an unknown function
  that receives a component, such as an unmodeled higher-order component, returns a value that may
  render that component with the element's props. When a root path renders such a value, the
  root phase requests the callee's module, so a parsed higher-order component can replace the
  assumption on the next round. The same applies to a rendered component whose declaration is
  initialized by a call, such as `const Panel = createPanel(...)`. Under an assumption, a component is explored
  only if the backward use walk found it on a use chain toward a factory callsite (or, when no walk
  ran, if its file is on the entry corridor), or if it receives JSX or callbacks. If the walk stops
  at its budget, components it did not reach can be skipped; the stop is reported as a gap. A
  component is explored once per entry input for the same props, which removes repeated paths
  that would only duplicate results; props too deep to fingerprint are always explored. Assumed
  rendering has a 1,000,000-step budget per entry input, set with the query's top-level
  `assumed_render_steps`; exhausting it is a coverage gap. Exact paths do not use this budget, so a
  larger one only finds more possible creations, at a cost in time.
- JSX can also leave the model inside a component that is parsed. On paths from configured roots,
  elements handed to an unknown or unsupported call, or referenced by an unsupported expression,
  are explored as possible with a `jsx_passed_to_an_unmodeled_call` gap. After a parsed component
  renders, JSX it received but did not render is explored as possible with a
  `jsx_dropped_by_a_partially_modeled_component` gap if its body reached anything the model could
  not follow. A fully modeled body that does not render a prop, such as one that returns `null`,
  keeps it unrendered.
- Class components: methods and function-valued class properties (`handleClick = () => {...}`)
  are modeled as methods. Reading one as a value, as in `onClick={this.handleClick}` or a render
  function child, gives a function bound to the instance's props.
- Records built from object literals and JSX props are closed: they have exactly the listed
  properties, so reading a missing one gives `undefined`, as in JavaScript, and a guard such as
  `onClose != null` on a prop that was not passed is decided. Defaults in destructuring and
  parameters apply when the value is `undefined`, and a component's `defaultProps`
  (`static defaultProps` or `Component.defaultProps = ...`) fill missing props as React does.
  Records that may have other properties stay open, and reading a missing property of one is
  unknown with a gap: entry props, the modeled factory result, context objects, props after a
  spread of an unknown value, props a higher-order component or configured wrapper passes, and
  props given by an assumed render. `Object.prototype` members are always unknown. An object
  literal spread copies a known record's properties in order, keeps each alternative of a union,
  and ignores `null` and `undefined`; spreading anything else makes the record open, and
  properties listed before that spread may have been overwritten.
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

Query report schema version 8 includes both stable source byte spans and file paths with one-based
line/column locations for factory calls and invocations. Each invocation now includes
`call_path`: ordered entry, call, render, modeled render, assumed render, factory, and invocation
locations from the explored path. Version 8 adds `possible` reachability and `assumed_render` path
steps. An invocation is reported once per callsite and projected argument values; `call_path` is
the first explored path and `other_paths` counts the rest. The `evidence` array keeps what the
report refers to (factory and invocation arguments, registrations, unresolved references, and gap
evidence paths) with ancestors up to 64 steps and every mutation step, so the evidence graph stays
navigable without every intermediate evaluation. The HTML view shows file transitions by default and can expand every same-file
step. The path is a modeled possibility; a factory call and later callback invocation need not
occur in one runtime stack. JSON also includes
`callsite_inventory`: syntactic calls found in parsed candidate files, labeled `analyzed`,
`filtered`, `unresolved`, or `skipped`. Inventory discovery scans every configured source for the
factory export name, then follows imports through re-export barrels to find renamed uses. This is
an independent check on the solver's creation rows, but dynamic property calls and aliases created
by assignments can still escape the syntactic inventory. The report gives configured and candidate
file counts plus any importer candidates skipped by the inventory budget. The text renderer uses
`path:line:column` and prints inventory counts; JSON retains locations and the evidence graph.
Each projected argument also has an optional evidence ID. The `gaps` array gives each coverage
gap a stable ID, kind, location when available, context choice, assessment, and links to affected
creations, argument projections, or inventory callsites. `direct` means the solver recorded
unresolved evidence for that creation or an unknown factory projection at that call. `may_affect`
is a possible relationship at an invocation or excluded callsite. `unknown_relevance` means the
gap's impact could not be established; `unlinked` means no source location or target was available.
The old `coverage.gaps` strings remain for compatibility. A gap link is static provenance, not
proof that an event executed at runtime.

Schema version 9 adds `component_boundaries`: the components on paths from configured roots that
the model could not follow, most consequential first. Each names the JSX tag, where linkage
stopped (`module` and `export` as written there), and a kind: `external_package` (an unparsed
package, such as one under `node_modules`), `unparsed_source` (a project file the text filter or
an expansion budget left out), `unresolved_import`, `dynamic_value` (a linked value the model does
not follow, such as an unknown higher-order component's result), `partially_modeled` (a parsed
component that dropped JSX after an unmodeled operation), or `escaped_jsx`.
`entered_from_reachable` marks boundaries an exact path reaches, which are the ones whose modeling
can make creations reachable; `affected_creations` and `sole_blocker_creations` count reported
possible creations below the boundary and those with no other assumption. Where a project
contract applies, `suggested_contract` gives TOML for it, with children forwarding, function-child
invocation, render props, and component props inferred from the props seen at the sites, or a
`component_wrappers` entry for a higher-order component. A contract is an assumption about the
component's behavior: check the component first. A contract on a parsed project component replaces
its body in the model, so factory calls inside it are no longer explored. The text renderer prints
the first ten boundaries with their suggestions, and the HTML report has a Boundaries tab.
Snapshot reuse is not implemented yet.

## Current analysis fragment

The engine follows object and array destructuring, local calls and closures, records, finite computed record
selection, arrays and finite spreads, exact string/boolean/numeric-enum strict-equality branches,
null comparisons, logical expressions, conditional expressions, finite-array `.map` and `.filter`,
`Object.values` on known records, local `.push`, object literal spreads, destructuring and
parameter defaults, `defaultProps`, JSX component props/children/spreads,
React `useMemo`, `useCallback`, `useState`, `memo`, `forwardRef`, and `createContext`,
`createElement` (and `jsx`/`jsxs` from `react/jsx-runtime`), `cloneElement`, `isValidElement`,
`Children.only`/`toArray`/`map`/`forEach`/`count`, `react-dom` `createPortal`, the built-in
`Fragment`, `Suspense`, `StrictMode`, and `Profiler` wrappers, `Object.assign` onto records and
components, prop renaming, wrapper callbacks, simple
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
Nullish guards narrow finite local alternatives inside their guarded branch, including guards
combined with `&&` or `||`.
`for...of` is explored for zero or one iteration and switch cases independently; both leave
coverage gaps because repeated iterations and fallthrough are not modeled. `.filter` evaluates
known predicates; unknown predicates retain possible subsets and leave a coverage gap. Finite
`.map` and `.filter` callbacks receive a concrete array index. `Object.values` retains values from
known records, but record key insertion order is not stored; records with multiple keys leave a
coverage gap when array order might matter. Arrays and records bound to locals share heap identity
across aliases and helper calls; local `.push` and static record-property writes update that
identity. Unknown branches retain up to 32 possible heap values; exceeding that budget produces a
coverage gap. Reassignment of captured
bindings and opaque calls receiving callback-bearing namespaces are conservatively unresolved.
Dynamic mutation targets, external mutation, live closure cells, full JavaScript coercion,
non-null loose comparisons, loops, and array methods
other than `.map` and `.filter` still require additional IR and solver work. Unsupported syntax can still hide creations or
dependencies; the current acceptance tests do not establish production absence guarantees.
