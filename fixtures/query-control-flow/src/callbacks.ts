export function invokeAction(
  callback: (actionKind: string) => void,
  actionKind: string,
) {
  callback(actionKind);
}
