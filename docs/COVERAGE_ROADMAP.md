# Detection coverage roadmap

The current factory-return query finds candidate creations, follows a selected returned
capability, and preserves unknown reachability and unresolved escapes. The next coverage work
should make missed callsites and result-changing unknowns easier to identify before adding more
JavaScript syntax. This is a proposed order, not a claim that a static trace proves runtime use.

## Recommended next slice: explain what each gap can affect

Coverage gaps are currently strings. A missing imported value, an opaque callback, and an
unsupported expression can all make a report incomplete, but their impact on a particular
creation is not always clear. Give gaps stable kinds and source locations, then attach each gap to
the factory argument, capability path, invocation argument, or callsite inventory entry it could
change. Report gaps that have no demonstrated path to a query result separately.

This is done when a user can open one unknown result, follow evidence to its first unsupported
edge, and see which conclusion might change if that edge were resolved. The report must keep a
gap when relevance itself is unknown. A lower gap count alone is not success.

## Then strengthen detection in this order

| Priority | Work | Coverage check |
| --- | --- | --- |
| 1 | Make the callsite inventory symbol aware beyond text hits and direct imports. Follow renamed exports, namespace members, and local aliases; distinguish a same-named decoy from a call to the configured factory. Keep an explicit `unexamined` or budget status for sources the inventory cannot classify. | Synthetic calls inside and outside the initial text filter all get the right status. A computed call or unresolved alias cannot silently support a complete-coverage claim. |
| 2 | Carry finite caller arguments through forwarding functions, local bindings, branches, and JSX spreads. Use a symbol-use index and backward slices so large importers do not require whole-file speculative execution. Preserve one caller context per creation. | Two callers passing different arrays produce separate, correctly paired factory and invocation values. A skipped dependency creates a gap at the affected argument. |
| 3 | Trace callbacks through common consumers and returned closures with versioned, project-configurable contracts. Record whether a callback is passed, registered, or actually called by a modeled path. | A callback handed to an opaque consumer stays possible or unresolved; a modeled consumer produces an invocation only through its specified argument and call path. |
| 4 | Extend local heap cells to computed keys, indexed writes, destructuring assignments, and live captured bindings. Treat getters, proxies, external mutation, and unknown alias sets conservatively. | A write through one alias is visible through another, including across a helper call. A later captured-binding reassignment is represented or explicitly unresolved. |
| 5 | Add bounded loop and exception flow with a monotone worklist, context-keyed summaries, and widening. Keep the zero-iteration path distinct from one or more iterations. | A factory or invocation reachable only in a loop or `finally` is found, while budget exhaustion remains visible and reproducible. |

Each slice needs adversarial fixtures: shadowed names, decoy modules, re-export cycles, partial
returns, unrelated opaque code, and budget limits. Tests should assert both the correlation of
known values and the exact place where certainty is lost. An independent inventory-to-creation
comparison is especially useful for finding false negatives.

## Scale needed to widen coverage

The inventory and solver should share a versioned per-file IR cache, resolved import index, and
context-keyed summaries. Cache keys must include source content, project resolution settings,
frontend version, and relevant query/model semantics. Benchmark cold and warm runs on broad
synthetic source trees, and compare reports byte-for-byte to uncached runs. Caching should allow
wider candidate discovery without hiding stale or skipped files.

## Interpretation boundary

`candidate_invocation` is evidence of a modeled call path. `absent_within_model` is limited to
the explored model and its coverage gaps. Neither result establishes whether an event or
callback actually ran in production. Keep exact values, conditional alternatives, and unknowns
separate in reports and tests.
