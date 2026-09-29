export function useHideableThing(
  _types: readonly string[],
): { markHandled: (hideKind: string) => void } {
  throw new Error("fixture implementation is selected by the query");
}
