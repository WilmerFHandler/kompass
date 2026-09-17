type Config = { enabled: boolean };

const moduleValue = initialize({ enabled: true } satisfies Config);
const moduleChoice = moduleValue ? moduleValue.ready : false;

export class Cache {
  static table = makeTable();
  state = hydrate();

  constructor(readonly key: string) {
    this.state = seedState(key);
  }

  get(value: string): string {
    return value ?? this.key;
  }

  static from(value: string): Cache {
    return new Cache(value);
  }
}

export const instance = new Cache("default");
