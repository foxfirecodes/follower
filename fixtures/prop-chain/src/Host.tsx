import { useCapability } from "./capability";

type LevelOneProps = { onProceed: () => void };
type LevelTwoProps = { onContinue: () => void };
type LevelThreeProps = { action: () => void };

export function Host({ variant }: { variant: "alpha" | "beta" }) {
  const { run } = useCapability(variant);
  return <LevelOne onProceed={run} />;
}

function LevelOne(props: LevelOneProps) {
  return <LevelTwo onContinue={props.onProceed} />;
}

function LevelTwo({ onContinue }: LevelTwoProps) {
  return <LevelThree action={onContinue} />;
}

function LevelThree({ action }: LevelThreeProps) {
  return <button onClick={() => action()}>Run</button>;
}

