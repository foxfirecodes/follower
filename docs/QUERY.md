# Factory-return invocation query

`follower query` runs a declarative, creation-centered query. Its first query kind answers:

1. Where is a configured factory symbol called?
2. What values reach selected factory argument positions?
3. Where does a selected property or tuple element of the returned value flow?
4. With what values is that capability invoked?

The query is independent of names such as `useWidgetActions`, `runAction`, or `ACTION_KIND`.
Those are matcher paths, argument indexes, and output labels in TOML.

The report answers these per callsite in `callsites`, whether or not a path from a configured root
reaches the callsite; reachability is a separate label on each entry. The text output and the HTML
report's first tab lead with this view, and `creations` keeps the per-context detail.

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

Use `--format csv` for a flat table, for agents and spreadsheets, with one row per call made with
the factory result and item it applies to:

```sh
follower query --project flow.toml --query query.toml --format csv > report.csv
follower view report.csv
```

`row_kind` is `call`, `excluded_call` (a call whose conditions match nothing the callsite
requests), `escape` (somewhere the result went that the walk does not follow), or `no_call` (an
item the callsite requests that no call applies to, or a callsite whose result is never called).
`item.<label>` holds one element of the first factory argument whose values are arrays, with
`item_complete` saying whether every element the call applies to is known, so a lookup by item is
a filter on one column. Each row also has the callsite's path, line, enclosing function,
`reachability`, `unreached_reason`, and `status`; the call's path and line, `arg.<label>` for each
projected invocation argument, `arguments_resolved`, and `found`: `explored` if an explored path
ran the call, `source` if only the source walk found it, or `inferred` if it was found through
props passed next to a module import. `context`, `via`, `instance`, and `conditions` give where
the call sits, how the result reached it, the instance whose items it uses, and the conditions on
its path; `unfollowed` counts the places the callsite's result went that the walk does not
follow, and `note` explains unknown values and other rows. Other factory arguments are
`factory.<label>`. Multiple values in a cell are joined with ` | `, values print as
`Enum.Member`, bare strings, or `?reason` for an unknown, and paths are relative to the
repository that holds them, found by the nearest `.git`. When a callsite has calls, its escapes
are one row each without an item; when it has none, each item it requests gets the escape rows.

`follower view report.csv` writes `report.html`, or `--output` elsewhere, as a self-contained,
owner-readable page built from the CSV alone, so it shows exactly what an agent reading the CSV
sees. It groups rows by item by default and can group by callsite, call file, argument, status, or
enclosing function, with search over the shown columns, filters, a needs-review view (rows whose
answer is not complete: a result not called or escaping, an escape whose calls are missing even at
a callsite with calls, an unknown argument, or items not all known), and a column picker. A search that narrows to three groups or fewer opens them, and
`report.html#q=Kind.A&group=callsite` opens with that search and grouping.

Add `--html-report` to write an interactive, self-contained report to a new owner-readable file
in `/tmp`. The command prints its path to stderr and leaves JSON or text stdout unchanged. Set
`[report] html = true` in the query TOML to generate it on every run. The report is also written
before a `--fail-on-unresolved` failure. It contains query values and paths, so treat the file as
private and remove it when finished.

For a hook that returns `[selectedItem, applyAction]`, select the second element with
`returned_index = 1` instead of `returned_property`. These selectors are mutually exclusive.
Use `scan_callback_bodies = true` at the top level of the query when you want calls inside
callbacks handed to opaque libraries or component props. Those calls are candidates: the report
keeps a coverage gap because the consumer might never invoke the callback. The option also runs,
after each component renders, the callbacks created during the render that carry the factory
result and that nothing in the model called, such as handlers in intrinsic props other than
`onClick` or in records passed elsewhere. When the backward use walk found a chain from a factory
callsite toward the entry, a callback that names a function on that chain runs too, such as a
click handler that calls a function opening a modal that holds a callsite. They run with unknown arguments and at most possible
reachability, and invocation paths mark the step as `uncalled_callback`. A call that a budget cut
short does not count as a call, and an unreached callsite's callbacks get their own render
budget, so a large subtree cannot use it all first.

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

An argument projection can read inside the argument and stand in for what is missing:

```toml
[[capability.invocation_arguments]]
index = 1
path = ["action"]      # the `action` of an options object passed second
default = "unknown"    # when the argument or the property is missing
label = "action_kind"
```

`path` reads properties of the argument, in explored and source values alike. `default` replaces
a missing value, alone or as one alternative; it is a string, integer, or boolean, or a module's
exported value, as `{ module = "src/kinds.ts", export = "ActionKind", path = ["UNKNOWN"] }`. A
module that is not parsed is requested, so the value is known from the next round. On a factory
argument, `items = true` says the argument names the items calls apply to: a single value counts
as a list of one, so it fills the `item.<label>` column as an array argument's elements do, and a
choice of values, as `ready ? Kind.A : Kind.B`, holds each.

For a function that acts when called, such as one that records an action directly, set
`call_is_invocation = true` under `[capability]` instead of a returned selector. Each call of the
factory is then itself the invocation: `invocation_arguments` select the call's own arguments,
nothing is followed after the call, and each callsite reports one call at the callsite. Labelling
its projections like another query's makes their CSVs share columns, and `follower view a.csv
b.csv` shows CSVs with the same columns as one table.

A factory may invoke its result itself, as a hook that calls the callback it returns when the
component unmounts. Each `[[capability.implicit_invocations]]` entry reports such an invocation as
a call at every callsite that does not opt out:

```toml
[[capability.implicit_invocations]]
arguments = { action_kind = "auto" }    # what it passes, by invocation argument label
unless_argument = 2                    # a truthy third factory argument opts out
except_items = { module = "src/config.ts", export = "KEEP_SHOWN" }
description = "called by the hook when the component unmounts"
```

Argument values are written as for `default`. Where the factory argument at `unless_argument` is
truthy on every explored path, or written as a truthy literal, the callsite makes no such call;
where that is not known, the call is reported with a note in `via`. Items in the exported list or
set of `except_items` are not ones the call applies to, so a callsite requesting only those gets an
`excluded_call`. When the callsite's arguments come from its scope's parameters, as in a wrapper
component, each instance of the scope decides from what it passes, and the call applies to that
instance's items.

`exclude_callsites` at the top level lists path globs of factory callsites the query leaves out,
as `["**/framework/**"]` for the factory's uses inside the code that implements it, whose calls
another query already reports.

Project configs can resolve repository aliases without a TypeScript compiler run:

```toml
schema_version = 1
name = "sample-app"
source_roots = [
  "src",
  "generated/itemKinds.ts",
]
source_contains_any = ["useItemSelection", "applyAction"]
# For a React Native run: try `Panel.ios.tsx`, then `Panel.native.tsx`, then `Panel.tsx`.
platform_extensions = [".ios", ".native"]
source_excludes = ["**/web/**", "**/*.web.tsx", "**/*.android.tsx"]

[import_aliases]
"@sample/*" = "src/*"

# Stores whose values are written in one place and read in others.
[[state_stores]]
kind = "zustand"
module = "zustand"
export = "create"

[[callback_selector_imports]]
module = "@sample/state"
export = "selectFromStore"
callback_argument = 1
```

Alias targets are relative to the project TOML directory unless absolute. Both exact names and
single-`*` patterns are supported. `platform_extensions` lists the suffixes an import tries, in
order, before the plain file, as a bundler for one platform does, so code with `Panel.web.tsx` and
`Panel.native.tsx` beside each other runs once per platform with its own project config. Without
them, `./Panel` resolves only to `Panel.tsx`, which in such code may be a type-checking stand-in
that re-exports one platform's file. `source_excludes` skips files in the directory walk by glob,
where `**` matches any number of path segments and `*` any characters within one, matched against
the full path; an excluded file is still parsed when an import resolves to it. Source roots can be directories or individual TS/JS files;
they are canonicalized before symbol linkage. `source_contains_any` is a fast text prefilter for
directory roots; explicitly listed files are always included. It lets a query search a
broad directory without parsing every source file. During a query, callback-bearing calls and JSX
props can pull excluded directly imported files into the analysis. Unknown factory arguments can
pull in the imported constants they depend on. A lightweight import scan also finds files that
import exported wrappers returning the tracked callback or exported functions/components whose
factory arguments are still unknown. It follows re-export barrels and evaluates direct imported
calls and JSX uses in large importer files. The analysis expansion is bounded at 256 additional
files and eight rounds, skips `node_modules`, and reuses parsed files within the run; the backward
use walk below has its own 256-file budget, and the root phase that follows what paths from the
entries read has 512. A `[limits]` table sets `discovery_files`, `backward_walk_files`, and
`root_phase_files` for a project whose runs need more or fewer. Importer
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

[[component_wrappers]]
module = "@sample/store"
export = "connect"
component_argument = 0
curried = true

[[lazy_component_factories]]
module = "@sample/lazy"
export = "loadComponent"
promise_property = "load"

# open(Panel, { onClose }), open(import('./Panel'), { onClose }), or open(loadPanel, { onClose })
[[component_openers]]
module = "@sample/overlay"
export = "open"
component_argument = 0
props_argument = 1

# openSheet(Panel, { props: { onClose } })
[[component_openers]]
module = "@sample/overlay"
export = "openSheet"
component_argument = 0
props_argument = 1
props_path = ["props"]

# openModal(async () => (props) => <Panel {...props} />)
[[component_openers]]
module = "@sample/overlay"
export = "openModal"
render_argument = 0

# Rendered somewhere no entry's path is found, such as a modal a store opens.
[[render_roots]]
module = "src/settings/SettingsModal.tsx"
export = "default"

# createSetting(id, { useNotice, render }): every call renders what its second argument holds.
[[render_calls]]
module = "@sample/settings"
export = "createSetting"
arguments = [1]

# const Stack = createStack(); <Stack.Screen component={Panel} /> or getComponent={() => Panel}
[[component_consumers]]
module = "@sample/navigation"
export = "createStack"
member = "Screen"
component_props = ["component"]
render_props = ["getComponent"]
invoke_children = true

[[component_consumers]]
module = "@sample/navigation"
export = "createStack"
member = "Navigator"
forward_children = true
```

A consumer contract explores the named render callback, component prop, children, or a
function-valued child as a possible render; a render callback may return the element or a component
to render. With `member`, the export is a factory and the contract applies to tags that are that
member of what it returns, as `Stack.Screen` for `const Stack = createStack()`; with
`curried = true`, of what the call of its result returns, as `createFactory(View)(config)`. It applies to a JSX tag imported from its module and
export, and to any tag whose import passes through that export on the way to its definition, so
a contract on a package export also covers barrels that re-export it under the same name. A wrapper contract says the returned component may
render the component at the given argument index; with `curried = true`, the export returns the
wrapper, as in `connect(mapState)(Panel)`, and the index is in the call of its result. The callsite
walk follows props of a wrapped component, such as `export default connect(mapState)(Panel)`,
into `Panel`. An opener contract describes a function that renders a component outside the
caller's render, such as a modal or sheet opener. `component_argument` holds the component, an
`import()` of its module, or a loader that returns either, called at the argument or passed itself;
`props_argument` holds its props, under `props_path` when they are nested. `render_argument` holds
a function that returns the element, or a component the opener renders. Exploration renders what the
opener opens as a configured component's render props are rendered, and the callsite walk follows
props into the opened component, which it finds where the opener is called. When the component
comes from a parameter, as in `open(props.importer(), props)`, the walk takes what the element or
call it entered the function through passes for it, so each caller's component gets only its
caller's path. The walk follows a render function's JSX in the source without a contract, since the
JSX is written where the result is. A lazy factory contract recognizes a callback
returning a literal `import()`, written inline or declared in the file under the name the property
holds (`load: importPanel`), and links its default export. These contracts describe possible
paths, not guaranteed
route matches, authorization, loading, or runtime rendering. Exploration takes an awaited value
as the value itself, so `import('./Panel')` gives the module's namespace when the module is parsed
(and requests it otherwise), as `require('./Panel')` does: `const { default: Panel } = await import('./Panel')` and
`import('./Panel').then((module) => module.Panel)` give the component, which a render-function
opener contract then renders; `Promise.resolve(value)` gives the value, and so does
`Object.freeze(value)`. Unconfigured imports remain unknown. The backward walk still parses
only candidate importer paths;
class `render()` methods and direct `this.method()` calls can supply use edges without indexing
every file.
Object-rest props, JSX fragments, and configured member tags can forward children through the
same slice. As in React, a single JSX child is passed as `children` itself and several children as
an array, so a component can call a function child. JSX text follows the React whitespace rule:
indentation-only text is dropped, and other text is a string child. Contexts created by React `createContext` need no contract: their `Provider` renders
its children and their `Consumer` calls its function child. A configured render callback name filter is intended
for focused traces; each skipped callback is reported as a coverage gap. Repeated renders at one
source site are bounded per component and callback identity and per set of JSX it receives, to 64
visits on exact paths and 16 on possible ones, with a coverage gap if the bound is reached. The
set includes JSX inside the elements and callbacks the site receives, so a wrapper used in many
places still renders the children given at each use, even when it hands them to a callback or
wraps them in another element. A component already rendered exactly with the same props for the
same entry input is not explored again: its creations stand, and what it reported to enclosing
components is replayed. Filtered `all_creations` queries postpone entry execution until import and
backward-use expansion has settled, avoiding repeated full entry walks during early rounds.

## Semantics

- Each result is centered on one dynamic creation context. Multiple finite root inputs at the same
  source callsite remain separate, so values from unrelated registry rows do not cross-pair. Exact
  paths that call the factory at the same callsite for the same root input with the same projected
  factory arguments share one creation, which lists the invocations of every such path.
- Projected values preserve strings, integral numbers, numeric enum members (name and value),
  arrays, finite array alternatives, alternatives of known literals (such as a string chosen by a
  condition), `null`, `undefined`, and explicit unknowns. A branch-built array remains a set of
  possible arrays instead of collapsing to one merged list. String enum members currently
  project to their string values. Alternatives that include an unknown value keep their known
  alternatives beside the unknown one, which still leaves the value unresolved. An array element
  chosen among values, such as a table entry read with an unknown key, contributes each of them as
  an item, except `null` and `undefined`.
- `reachable` reports creations explored from configured entry points and finite input domains.
- When no path from an entry reaches some code, the project can declare that it is rendered. A
  `[[render_roots]]` entry names a component, function, or module binding such as a registry
  record, by file and export like an entry, that exploration starts from with unknown props or
  arguments; like an entry, it must resolve, or the query fails. A `[[render_calls]]` entry names an
  imported function whose calls render what they are given: at every call of it in the parsed
  files, wherever the call is, the listed arguments are rendered, with components rendered, functions
  called with unknown arguments and what they return rendered, and records and arrays searched;
  locals around the call are unknown. Both run after the entries as possible renders, and what they
  reach has `reachability = "declared"` and a `declared_render` first path step, so an answer that
  rests on a declaration says so. A declared render is an assumption about the project: check it.
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
`call_path`: ordered entry, call, render, modeled render, assumed render, uncalled callback,
factory, and invocation locations from the explored path. Version 8 adds `possible` reachability
and `assumed_render` path steps. An invocation is reported once per callsite and projected
argument values; `call_path` is the first explored path and `other_paths` counts the rest. The `evidence` array keeps what the
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

Schema version 9 adds `component_boundaries` and `unreached_callsites`. `component_boundaries`
lists the components on paths from configured roots that the model could not follow, most
consequential first. Each names the JSX tag, where linkage stopped (`module` and `export` as written there), and a kind: `external_package` (an unparsed
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

`unreached_callsites` explains each matching factory callsite that no exact or possible path
reached. From the function containing the callsite, it walks static uses (calls, JSX, imports, and
`import()`) back to the nearest user that exact exploration ran, and names the use inside it that
was not followed. The `reason` is `callback` (inside a callback passed to a call, such as a modal
opener or an effect, that was not invoked), `prop_callback` (inside a function passed as a JSX
prop), `local_callback`, `lazy_import`, `render_budget` (the render visit budget stopped
exploration at the use), `component_not_followed` (the element was rendered but its component's
body was not explored, such as the result of an unknown lazy loader), `created_not_rendered`,
`branch_not_taken`, or `no_explored_ancestor`, where `detail` names the code at which the use chain
ends. `explored_ancestor` and `blocking_site` give that user and use, and `chain` the code between,
innermost first. If the chain passes a module binding initialized by a loader call whose record
argument returns a dynamic import, `suggested_contract` gives a `lazy_component_factories` entry
for the loader. The walk is static and stops after 4,000 nodes, so it names a likely blocker rather
than proving that no path exists. The text renderer groups callsites by reason and blocking use,
and the HTML report has an Unreached tab.

Schema version 10 adds `callsites`, one entry per matching factory callsite. `factory_arguments`
lists the distinct values of each projected argument across the callsite's explored contexts; a
callsite no context explored reports the arguments it writes when they do not depend on local
values. `reachability` is the strongest tier among its creations, `unknown` with an
`unreached_reason` when no path reached it. `capability` describes what became of the selected
result. The walk follows it through the source from the factory call: through destructuring and
aliases, `useCallback`, `useMemo`, and `useRef` (whose `current` holds its argument), property
writes such as `ref.current = value`, record and array fields, JSX props into function and class
components (including components loaded by `lazy(() => import(...))`, a loader whose record argument
returns `import(...)`, or a local bound to `await import(...)` or destructured from it, as in
`<module.default />` or `const { default: Panel } = await import('./Panel')`),
arguments to project functions, and `return` to each caller of a hook. A call of a parameter, such
as `children({ apply })` or an injected `open({ onClose })`, is followed into the function each
caller passes for it, written at the call or bound to a name, including through `useCallback`. Props
passed next to a module import, as in `openSheet(import('./Sheet'), { onClose })`, are taken as
props of the component the module exports; the path step names the call and module, since this is
inferred from the call's shape. Callers match through any import name, and a name destructured from
a namespace, as in `export const { useNotice } = web` over `import * as web`, is the declaration it
re-exports. When a hook's or parameter's callers are not parsed, the walk asks the next round to
parse the files that import it, and when a component or function it meets is in a file that is not
parsed, such as one the text filter skipped, it asks for that file, within the root phase's file
budget. A value stored and read back elsewhere is followed from the write to each read: what
`useState` starts with or a setter or updater writes reaches the state in the same component, what
`this.setState` writes reaches `this.state` in the class's methods, a context provider's `value`
reaches each `useContext` of that context, and, for a store a `[[state_stores]]` entry describes,
what `S.setState` writes reaches `S(selector)`, `S.getState()`, and `useStore(S, selector)`
wherever they are, with the selector's property path taken into the state. A component an opener
loads from a property of a value the walk cannot trace, as `selected.importer` for an entry picked
from a table, is any value the caller's file writes under that property; those calls are
`inferred`. In the same way, when the contexts explored through an instance give no known elements
for a factory argument, what the instance's caller writes for it is evaluated with such a property
read taken as each value the file writes under it, so `contentTypes={[selected.id]}` requests every
`id` the table names. A prop the entered site spreads from its own props, as `<Sheet {...props} />`, is
followed to where that component's caller wrote it. An argument read from a ref, as
`pending.current`, takes the ref's initial value and the values the file assigns to it when nothing
else is known. Each call made with the result
is in `calls`, with the functions it sits in (`context`, such as `the onClose prop of <Panel>`), how
the result reached it (`via`, such as `prop onClose of <Panel>` or `returned by useNotice to
Banner`), and the values of each projected invocation argument. An argument that reads no local
values, such as an enum member or a literal, is evaluated from module bindings, so it is known even
if no explored path executed the call; otherwise the values come from explored invocations at the
same call, or are unknown with the reason, such as a parameter of the enclosing function. `explored`
says whether an explored path executed the call. Passing the result as an intrinsic element's event
handler is a call with an unknown event argument. `escapes` lists where the result went that the
walk does not follow, such as an unknown function, a store, a property write, a component it cannot
resolve, or a hook whose callers are not in the parsed files; an escape that several paths reach
is listed once, with the first path's `via`. `status` is `called`,
`called_with_unknown_arguments`, `escapes`, `not_called` (used, for example passed to code that
ignores it, but never called), or `unused` (not bound, or bound and never used). For an array
argument with several possible arrays, such as a list filtered by conditions the model cannot
decide, `possible_elements` lists the elements they hold; the text output and the HTML report show
that list and the number of arrays instead of every array when there are more than four. Where a
factory argument stays unresolved, `possible_elements` also lists what a local array built from
literal elements and `push` calls may contain, such as the types pushed under conditions too many to
keep as separate arrays, or a choice between such arrays, as in `hidden ? [] : items`, and `values_from_callers` gives the argument's value with what each caller
passes, when it reads only the enclosing function's parameters and a caller passes values that do
not depend on its own locals; up to 32 callers are evaluated, one level up. Each call also says
which elements of an array factory argument it can apply to, in `elements`. For a call through an
instance that no explored path renders, they are what the instance's caller writes for the
argument, such as `kinds={kinds}` with `const kinds = disabled ? [] : [Kind.A]` in the caller; an
element that reads the caller's own parameters, as `kinds={[kind]}`, is what the caller's callers
pass for them. The walk keeps the
conditions each step of the path runs under: `if` branches, the code after an early return, `?:`
branches, and the right side of `&&` and `||`. A condition that compares with members of the
elements' enum, such as `case Kind.A:` or `visible === Kind.A`, narrows them; `guards` lists the
sets one compared value must be in, and `guards_not` the members ruled out, as by `if (visible !==
Kind.A) return`. When the result left the callsite's scope through a caller, such as the element
that renders a wrapper with a function child, `instance` is that caller site, and the elements start
from what that instance requests: the contexts explored through it, or a literal it passes.
`elements_complete` is false when those elements are not all known; the conditions still bound them.
A call whose conditions match no element the callsite requests cannot be reached with this
callsite's result, as when a shared descriptor carries several hooks' callbacks and a switch picks
the component, and moves to `excluded_calls`. A function that passes its own parameter on, such as
`(kind = Kind.DEFAULT) => { track(); apply(kind); }`, is a wrapper: the walk follows the wrapper
too, and each call of it reports the value it passes, or the parameter's default when it passes
none, in place of the call inside the wrapper. Such a call counts as `explored` when the call inside
the wrapper saw its values on an explored path. The walk visits at most 400 scope and target pairs
and 8 scope changes per callsite.

Schema version 11 adds `declared` reachability and `declared_render` path steps, for creations
reached only from a render root or render call the project declares, described below.

Snapshot reuse is not implemented yet.

## Current analysis fragment

`new Set(iterable)` holds the iterable's members, and `includes` or `has` on a known array or
such a set compares like `===`: it is decided when one element matches or every comparison is.
`a && b` yields `a` only when `a` is falsy, and `a || b` only when it is truthy, so that operand
keeps its truthiness even when its value is unknown; `unknown && false` is falsy, which lets a
filter drop an element its other conditions exclude.

Optional chains read and call like their plain forms: `value?.name` is `value.name` and
`callback?.(argument)` is `callback(argument)`. A property read on a `null` or `undefined`
alternative of a value that may be something else gives `undefined`, which is what an optional
chain gives; a plain read would throw instead, so its value is never used. A read on a value that
is only `null` or `undefined` is unknown, since such a value often stands for one set later.

The engine follows object and array destructuring, local calls and closures, records, finite computed record
selection, object literals with computed keys such as `{ [Kind.A]: value }`, `void` expressions,
JSX attributes written without a value, which pass `true`, arrays and finite spreads, exact string/boolean/numeric-enum strict-equality branches,
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
remains unknown, and projected factory captures containing unknown values, including joins with
an unknown side, carry gaps; finite alternatives of literals or arrays do not.
Nullish guards and plain truthiness tests narrow finite local alternatives inside their guarded
branch, including nested alternatives and guards combined with `&&` or `||`.
A computed read whose key is not known, as in `table[key]`, gives any of the object's values or
`undefined`, for an object or array of up to 64 entries; on an open record it may also be an
unknown. A computed key that is not known, as in `{ [key]: value }`, leaves the literal's other
names unknown, so a read of any name may give that value. Repeated arrays count once toward the
budget of 64 for a spread of several possible arrays.

A value imported from a module that is not parsed is unknown with the reason
`unparsed_module:<specifier>`, which property reads and calls keep. When such a value reaches a
callsite's factory arguments, the next round parses that module, as it does for the imports a
callsite's arguments depend on, including what local `.push` calls add to them. A reason that
remains names the module that was not parsed.
`for`, `for...in`, `for...of`, and `while` bodies are explored for zero or one iteration, a
`do...while` body for one, and switch cases independently; these leave coverage gaps because
repeated iterations and fallthrough are not modeled. Blocks keep their own scope, labels are
transparent, and `break` and `continue` end nothing, since each body is explored on its own. A
`try` block and its `catch` handler are explored as alternatives, with an unknown caught value,
followed by the `finally` block. `throw` ends its path, so bindings assigned on a branch that
throws do not reach the code after it. `.filter` evaluates
known predicates; unknown predicates retain possible subsets and leave a coverage gap. Finite
`.map` and `.filter` callbacks receive a concrete array index. `Object.values` retains values from
known records, but record key insertion order is not stored; records with multiple keys leave a
coverage gap when array order might matter. Arrays and records bound to locals share heap identity
across aliases and helper calls; local `.push` and static record-property writes update that
identity. Unknown branches retain up to 32 possible heap values; exceeding that budget produces a
coverage gap. A function calls itself at most 8 levels deep on one path, or 3 when its arguments
are not all known, so recursion over unknown data stops early, with a coverage gap, instead of
repeating the same exploration at every level, which grows with each branch that recurses. A push through a property path, as in `record.items.push(value)`, appends to the
array there, including after a branch joins several records; other methods that change an array
in place (`unshift`, `splice`, `pop`, `shift`, `sort`, `reverse`, `fill`, `copyWithin`) leave it
unknown with a coverage gap. Reassignment of captured
bindings and opaque calls receiving callback-bearing namespaces are conservatively unresolved.
Dynamic mutation targets, external mutation, live closure cells, full JavaScript coercion,
non-null loose comparisons, repeated loop iterations, and array methods
other than `.map` and `.filter` still require additional IR and solver work. Unsupported syntax can still hide creations or
dependencies; the current acceptance tests do not establish production absence guarantees.
