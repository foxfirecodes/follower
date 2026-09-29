export function useHideableThing(_types: readonly string[]) {
  return { markHandled: (_hideKind: string) => undefined };
}
