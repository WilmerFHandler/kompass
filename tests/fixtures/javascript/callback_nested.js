export function callbackOwners(items, enabled) {
  if (!enabled) {
    return [];
  }

  return items.map((item) => {
    if (item.value > 0) {
      return save(item.value);
    }
    return fallback(item);
  });
}
