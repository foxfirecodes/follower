import { useThing as makeWidget } from "./public";
import * as Widget from "./public";
import { useWidgetActions as decoy } from "./decoy";

enum ThingType {
  Banner = "banner",
  Modal = "modal",
  Upgrade = "upgrade",
}

enum ActionKind {
  CloseButton = "close_button",
  Timeout = "timeout",
}

const registry = {
  welcome: {
    types: [ThingType.Banner, ThingType.Modal],
    View: WelcomeCard,
  },
  upgrade: {
    types: [ThingType.Upgrade],
    View: UpgradeCard,
  },
};

export function Host({ variant }: { variant: "welcome" | "upgrade" }) {
  const item = registry[variant];
  const { runAction } = makeWidget(item.types);
  const View = item.View;
  return <View dismiss={runAction} />;
}

function WelcomeCard({ dismiss }: { dismiss: (kind: string) => void }) {
  return <Frame onDismiss={dismiss} />;
}

function Frame({ onDismiss }: { onDismiss: (kind: string) => void }) {
  return (
    <button onClick={() => onDismiss(ActionKind.CloseButton)}>Close</button>
  );
}

function UpgradeCard({ dismiss }: { dismiss: (kind: string) => void }) {
  return <button onClick={() => dismiss(ActionKind.Timeout)}>Later</button>;
}

// Deliberately absent from the configured render roots. An all_creations query
// still reports this callsite and labels its reachability as unknown.
function DormantCard() {
  const { runAction } = Widget.useThing([ThingType.Modal]);
  return (
    <button onClick={() => runAction(ActionKind.Timeout)}>Hide</button>
  );
}

// Same exported spelling, different canonical declaration: this must not match
// the query's factory identity.
function DecoyCard() {
  const { runAction } = decoy([ThingType.Banner]);
  return <button onClick={() => runAction(ActionKind.Timeout)}>No-op</button>;
}
