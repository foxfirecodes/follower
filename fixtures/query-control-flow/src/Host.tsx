import { WidgetRow as Row } from "./rows";

enum ThingType {
  Banner = "banner",
  Modal = "modal",
  Upgrade = "upgrade",
}

enum ActionKind {
  CloseButton = "close_button",
  Timeout = "timeout",
}

export function Host({ variant, unknownRows }: {
  variant: "alpha" | "beta";
  unknownRows: readonly string[];
}) {
  // An unrelated unknown iteration should not make this callback query incomplete.
  unknownRows.map((row) => row);

  const rows =
    variant === "alpha"
      ? [
          { types: [ThingType.Banner], actionKind: ActionKind.CloseButton },
          { types: [ThingType.Modal], actionKind: ActionKind.Timeout },
        ]
      : [
          { types: [ThingType.Upgrade], actionKind: ActionKind.CloseButton },
        ];

  return rows.map((row) => (
    <Row types={row.types} actionKind={row.actionKind} />
  ));
}
