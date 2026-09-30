# Follower prototype

Follower is an executable experiment for a local Rust analyzer that explains callback
flow through TypeScript/React code. It intentionally implements a narrow semantic fragment first:
Oxc parsing/binding/CFG extraction, Node/TypeScript import resolution, an owned lowering boundary,
repository callback-factory models, finite registry choices, React component prop forwarding, and
intrinsic event registration.

The reference experiment is configured in `fixtures/reference/flow.toml`.

```sh
cargo run --bin follower -- index --project fixtures/reference/flow.toml
cargo run --bin follower -- audit --project fixtures/reference/flow.toml --model notice-dismiss-v1
```

The first generic query family finds factory calls, projects selected factory arguments, follows a
selected property of the returned value, and projects arguments at every invocation. The fixture
uses an array-valued factory argument and forwards the returned callback through React props:

```sh
cargo run --bin follower -- query \
  --project fixtures/factory-query/flow.toml \
  --query fixtures/factory-query/query.toml
```

Use `--format json` for the stable machine-readable report. The query declaration and current
semantics are documented in [`docs/QUERY.md`](docs/QUERY.md).

The control-flow fixture exercises finite strict-equality branches and row-preserving literal-array
`.map`, canonical ordinary-call linkage, and local callback reassignment:

```sh
cargo run --bin follower -- query \
  --project fixtures/query-control-flow/flow.toml \
  --query fixtures/query-control-flow/query.toml
```

The mutation fixture demonstrates the current conservative boundary for an aliased record whose
callback property is overwritten:

```sh
cargo run --bin follower -- query \
  --project fixtures/query-mutation/flow.toml \
  --query fixtures/query-mutation/query.toml
```

To exercise a callback forwarded through three components with prop renaming and two independent
creation contexts:

```sh
cargo run --bin follower -- audit --project fixtures/prop-chain/flow.toml \
  --model prop-chain-callback --scope reachable
```

The checked-in fixture distinguishes `() => dismiss()` from `() => dismiss` and is the first
target for the correlation-aware audit. Unsupported syntax is represented explicitly in the owned
IR and must become a query-specific coverage gap when it can affect a result.

Current implementation boundaries and experiment results are recorded in
[`docs/PROTOTYPE_STATUS.md`](docs/PROTOTYPE_STATUS.md).
