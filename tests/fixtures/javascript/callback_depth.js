export function nestedDepth(value) {
  if (value) {
    return [value].map((entry) => {
      if (entry) {
        return entry;
      }
      return null;
    });
  }
  return [];
}
