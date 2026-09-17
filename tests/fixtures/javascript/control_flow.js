export function plain(value) {
  return value;
}

export function optional(value) {
  return value?.child;
}

export function nullish(value) {
  return value ?? "fallback";
}

export function ternary(value) {
  return value ? "yes" : "no";
}

export function switching(value) {
  switch (value) {
    case 0:
      return "zero";
    case 1:
      return "one";
    default:
      return "other";
  }
}

export function catching(value) {
  try {
    return parseValue(value);
  } catch (error) {
    return recover(error);
  }
}
