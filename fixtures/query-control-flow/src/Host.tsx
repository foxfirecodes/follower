import { HideableRow as Row } from "./rows";

enum ThingType {
  Banner = "banner",
  Modal = "modal",
  Upgrade = "upgrade",
}

enum HideKind {
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
          { types: [ThingType.Banner], hideKind: HideKind.CloseButton },
          { types: [ThingType.Modal], hideKind: HideKind.Timeout },
        ]
      : [
          { types: [ThingType.Upgrade], hideKind: HideKind.CloseButton },
        ];

  return rows.map((row) => (
    <Row types={row.types} hideKind={row.hideKind} />
  ));
}
