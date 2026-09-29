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
- `candidate_invocation` means the selected capability reached a modeled call or React event path.
  `absent_within_model` means no invocation or unresolved escape was found in the explored scope.
  `unresolved` means an unsupported or opaque operation prevents that claim.
- `--fail-on-unresolved` exits unsuccessfully when a creation's conclusion is `unresolved`.

The report includes source byte spans and an evidence graph. Line/column rendering and snapshot
reuse are not implemented yet.

## Current analysis fragment

The engine follows destructuring, local calls and closures, records, finite computed record
selection, arrays, JSX component props/spreads, prop renaming, wrapper callbacks, and intrinsic
`onClick` handlers. Symbol matching resolves named imports to the configured module and also
recognizes a call from inside the defining module. Re-exports, namespace-member calls, mutations,
branches, loops, and array methods such as `.map` still require additional IR and solver work.
