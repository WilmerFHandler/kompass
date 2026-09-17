class Left {
  duplicate(value) {
    const branch = value ? left(value) : right(value);
    const result = branch ?? fallback();
    return result;
  }
}

class Right {
  duplicate(value) {
    const branch = value ? left(value) : right(value);
    const result = branch ?? fallback();
    return result;
  }
}

export function caller(value) {
  return new Left().duplicate(value);
}
