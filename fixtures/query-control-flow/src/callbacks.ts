export function invokeHide(
  callback: (hideKind: string) => void,
  hideKind: string,
) {
  callback(hideKind);
}
