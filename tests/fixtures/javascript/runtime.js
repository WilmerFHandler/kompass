export function normalize(value) {
  const label = value?.label ?? "unknown";
  return value && value.active ? label.trim() : null;
}

export const run = (items) =>
  items
    .filter((item) => item?.active ?? false)
    .map((item) => normalize(item));
