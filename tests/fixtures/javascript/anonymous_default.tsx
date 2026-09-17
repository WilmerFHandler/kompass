type Props = { value?: number };

export default ({ value }: Props) => (
  <Text>{value == null ? "empty" : value.toString()}</Text>
);

export const named = (value: number) => value ?? 0;
