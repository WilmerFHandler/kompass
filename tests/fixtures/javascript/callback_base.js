export function callbackOwners(items, enabled) {
  if (!enabled) {
    return [];
  }

  return items.map((item) => item.value);
}
