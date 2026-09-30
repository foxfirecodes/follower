// These deliberately collide by local name with the imported helper and component.
// Canonical symbol linkage must keep them out of the reachable call graph.
export function invokeAction(_callback: (value: string) => void, _value: string) {}

export function WidgetRow() {
  return <div />;
}
