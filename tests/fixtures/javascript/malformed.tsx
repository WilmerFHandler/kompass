export function Broken({ value }: { value: string }) {
  return <View>{value ? <Text>{value}</Text> : null;
}
