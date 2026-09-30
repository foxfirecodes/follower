export function useWidgetActions(
  _types: readonly string[],
): { runAction: (actionKind: string) => void } {
  throw new Error("fixture implementation is selected by the query");
}
