import * as CallbackFns from "./callbacks";
import { useWidgetActions } from "./widget";

enum ActionKind {
  CloseButton = "close_button",
  Timeout = "timeout",
}

export function WidgetRow({ types, actionKind }: {
  types: readonly string[];
  actionKind: string;
}) {
  const { runAction } = useWidgetActions(types);
  let dismiss = runAction;
  dismiss = (kind) => runAction(kind);
  if (actionKind === ActionKind.CloseButton) {
    return (
      <button
        onClick={() => CallbackFns.invokeAction(
          dismiss,
          ActionKind.CloseButton,
        )}
      >
        Close
      </button>
    );
  }
  return (
    <button
      onClick={() => CallbackFns.invokeAction(dismiss, ActionKind.Timeout)}
    >
      Later
    </button>
  );
}
