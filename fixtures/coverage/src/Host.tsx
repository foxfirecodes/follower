import { makeCapability } from "./factory";

const registry = {
  opaque: { key: "opaque", View: OpaqueConsumer },
  discarded: { key: "discarded", View: DiscardedElement },
};

export function Host({ variant }: { variant: keyof typeof registry }) {
  const item = registry[variant];
  const { callback } = makeCapability(item.key);
  const View = item.View;
  return <View onAction={callback} />;
}

function OpaqueConsumer({ onAction }: { onAction: () => void }) {
  consume(onAction);
  return <span />;
}

function DiscardedElement({ onAction }: { onAction: () => void }) {
  <button onClick={() => onAction()} />;
  return <span />;
}

