class Left {
  duplicate(value) {
    const branch = value ? left(value) : right(value);
    const result = branch ?? fallback();
    const normalized = normalize(result, value);
    const validated = validate(normalized, value, branch);
    return validated;
  }
}

class Right {
  duplicate(value) {
    const branch = value ? left(value) : right(value);
    const result = branch ?? fallback();
    const normalized = normalize(result, value);
    const validated = validate(normalized, value, branch);
    return validated;
  }
}

export function caller(value) {
  return new Left().duplicate(value);
}
