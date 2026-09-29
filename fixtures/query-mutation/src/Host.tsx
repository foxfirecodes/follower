import { useHideableThing } from "./hideable";

enum ThingType {
  Banner = "banner",
}

enum HideKind {
  Timeout = "timeout",
}

export function Host() {
  const { markHandled } = useHideableThing([ThingType.Banner]);
  const handlers = { run: markHandled };
  const alias = handlers;

  handlers.run = (kind) => markHandled(kind);

  return (
    <button onClick={() => alias.run(HideKind.Timeout)}>Later</button>
  );
}
