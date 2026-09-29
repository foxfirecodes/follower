# Rust code-flow explorer prototype

This repository is an executable experiment for a local Rust analyzer that explains callback
flow through TypeScript/React code. It intentionally implements a narrow semantic fragment first:
Oxc parsing/binding/CFG extraction, Node/TypeScript import resolution, an owned lowering boundary,
repository callback-factory models, finite registry choices, React component prop forwarding, and
intrinsic event registration.

The reference experiment is configured in `fixtures/reference/flow.toml`.

```sh
cargo run -- index --project fixtures/reference/flow.toml
cargo run -- audit --project fixtures/reference/flow.toml --model notice-dismiss-v1
```

To exercise a callback forwarded through three components with prop renaming and two independent
creation contexts:

```sh
cargo run -- audit --project fixtures/prop-chain/flow.toml \
  --model prop-chain-callback --scope reachable
```

The checked-in fixture distinguishes `() => dismiss()` from `() => dismiss` and is the first
target for the correlation-aware audit. Unsupported syntax is represented explicitly in the owned
IR and must become a query-specific coverage gap when it can affect a result.

Current implementation boundaries and experiment results are recorded in
[`docs/PROTOTYPE_STATUS.md`](docs/PROTOTYPE_STATUS.md).
