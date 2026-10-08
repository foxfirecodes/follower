# Follower

Follower reads a TypeScript/React codebase and tells you where a callback ends up being called,
and with what arguments. It works from the source code alone and never runs your app.

> Follower is an early prototype. It understands a limited set of TypeScript and React patterns,
> and it reports any code it can't follow instead of guessing.

## Why

In a React app, a callback can go a long way between where it's made and where it's used. A hook
creates it, a component passes it down as a prop, another component gives it a new name, and in
the end a button calls it from `onClick`. Searching for the callback's name misses most of those
steps.

Follower tracks the callback itself, whatever it's called along the way. You tell it which
function creates the callback, and it lists every place the callback gets called and the
arguments it gets. Anywhere it can't follow the callback, it says so, so you know what to check
by hand.

## Example

```tsx
export function App() {
  const { runAction } = useWidgetActions(["banner", "modal"]);
  return <Card dismiss={runAction} />;
}

function Card({ dismiss }: { dismiss: (kind: string) => void }) {
  return <button onClick={() => dismiss("close_button")}>Close</button>;
}
```

Ask Follower about `runAction` from `useWidgetActions`, and it reports that `runAction` was
created with `["banner", "modal"]` and is called with `"close_button"` from the button in `Card`,
even though `Card` calls it `dismiss`.

## How to use

You need Rust 1.96 or newer. Install the `follower` command from this folder:

```sh
cargo install --path .
```

### 1. Describe your project

Create a `flow.toml` that says where your source code is and where your app starts:

```toml
schema_version = 1
name = "my-app"
source_roots = ["src"]

[[entries]]
module = "src/App.tsx"
export = "App"
```

### 2. Write a query

Create a `query.toml` that names the function that creates the callback and the arguments you
want to see:

```toml
schema_version = 1
id = "widget-actions"
kind = "factory_return_invocations"
scope = "all_creations"

# The function that creates the callback
[factory]
project = "my-app"
module = "src/widget.ts"
export = "useWidgetActions"

# Record its first argument
[[factory_arguments]]
index = 0
label = "widget_types"

# Follow the runAction property of what it returns
[capability]
returned_property = ["runAction"]

# Record the first argument of every call to it
[[capability.invocation_arguments]]
index = 0
label = "action_kind"
```

### 3. Run it

```sh
follower query --project flow.toml --query query.toml
```

This prints a summary you can read. Other output formats:

- `--format json` gives the full report as JSON.
- `--format csv` gives one row per call, which works well in a spreadsheet. Run
  `follower view report.csv` to turn the CSV into a web page you can search and filter.

To try Follower without writing any files, run the example included in this repository:

```sh
follower query --project fixtures/factory-query/flow.toml --query fixtures/factory-query/query.toml
```

Run `follower --help` to see the other commands.

## Learn more

- [`docs/QUERY.md`](docs/QUERY.md) lists every project and query option and explains each part
  of the report.
- [`docs/PROTOTYPE_STATUS.md`](docs/PROTOTYPE_STATUS.md) lists what Follower supports today and
  where it stops.
