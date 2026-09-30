import { useWidgetActions } from "./widget";

enum ThingType {
  Banner = "banner",
}

enum ActionKind {
  Timeout = "timeout",
}

export function Host() {
  const { runAction } = useWidgetActions([ThingType.Banner]);
  const handlers = { run: runAction };
  const alias = handlers;

  handlers.run = (kind) => runAction(kind);

  return (
    <button onClick={() => alias.run(ActionKind.Timeout)}>Later</button>
  );
}
