export function render(value: boolean) {
  if (!value) {
    return null;
  }
  return <Text>ready</Text>;
}
