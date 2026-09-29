import * as CallbackFns from "./callbacks";
import { useHideableThing } from "./hideable";

enum HideKind {
  CloseButton = "close_button",
  Timeout = "timeout",
}

export function HideableRow({ types, hideKind }: {
  types: readonly string[];
  hideKind: string;
}) {
  const { markHandled } = useHideableThing(types);
  let dismiss = markHandled;
  dismiss = (kind) => markHandled(kind);
  if (hideKind === HideKind.CloseButton) {
    return (
      <button
        onClick={() => CallbackFns.invokeHide(
          dismiss,
          HideKind.CloseButton,
        )}
      >
        Close
      </button>
    );
  }
  return (
    <button
      onClick={() => CallbackFns.invokeHide(dismiss, HideKind.Timeout)}
    >
      Later
    </button>
  );
}
