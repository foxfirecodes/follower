import { useNotice } from "./notice";

enum Notice {
  Welcome = "welcome",
  Upgrade = "upgrade",
}

type DismissProps = { onDismiss: () => void };

const registry = {
  welcome: { key: Notice.Welcome, View: WelcomeCard },
  upgrade: { key: Notice.Upgrade, View: UpgradeCard },
};

export function Host({ variant }: { variant: keyof typeof registry }) {
  const item = registry[variant];
  const { dismiss } = useNotice(item.key);
  const View = item.View;
  return <View onDismiss={dismiss} />;
}

function WelcomeCard(props: DismissProps) {
  return <Frame onDismiss={props.onDismiss} />;
}

function Frame({ onDismiss }: DismissProps) {
  return <CloseButton onDismiss={onDismiss} />;
}

function CloseButton({ onDismiss }: DismissProps) {
  return <button onClick={() => onDismiss()}>Close</button>;
}

function UpgradeCard({ onDismiss }: DismissProps) {
  return <button onClick={() => onDismiss}>Close</button>;
}
