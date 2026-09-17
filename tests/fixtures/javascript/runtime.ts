type Item = { label?: string; active?: boolean };

export function normalize(value: Item | null): string | null {
  const label: string = value?.label ?? "unknown";
  return value && value.active ? label.trim() : null;
}

export const run = (items: Item[]): Array<string | null> =>
  items
    .filter((item: Item) => (item?.active ?? false) as boolean)
    .map((item: Item) => normalize(item));
