import { useHideableThing } from "./hideable";

enum ThingType {
  Banner = "banner",
  Modal = "modal",
  Upgrade = "upgrade",
}

enum HideKind {
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
  const { markHandled } = useHideableThing(item.types);
  const View = item.View;
  return <View dismiss={markHandled} />;
}

function WelcomeCard({ dismiss }: { dismiss: (kind: string) => void }) {
  return <Frame onDismiss={dismiss} />;
}

function Frame({ onDismiss }: { onDismiss: (kind: string) => void }) {
  return (
    <button onClick={() => onDismiss(HideKind.CloseButton)}>Close</button>
  );
}

function UpgradeCard({ dismiss }: { dismiss: (kind: string) => void }) {
  return <button onClick={() => dismiss(HideKind.Timeout)}>Later</button>;
}

// Deliberately absent from the configured render roots. An all_creations query
// still reports this callsite and labels its reachability as unknown.
function DormantCard() {
  const { markHandled } = useHideableThing([ThingType.Modal]);
  return (
    <button onClick={() => markHandled(HideKind.Timeout)}>Hide</button>
  );
}
