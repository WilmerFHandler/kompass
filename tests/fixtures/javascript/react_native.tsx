import React, { useCallback, useState } from "react";
import { Button, Text, View } from "react-native";

type Item = { id: string; enabled?: boolean; label: string };
type Props = { items: Item[]; onPick?: (item: Item) => void };

const DEFAULT_LIMIT: number = 20;
const nativeConfig = configureNative({ limit: DEFAULT_LIMIT });

export function NativeScreen({ items, onPick }: Props): React.ReactElement {
  const [error, setError] = useState<Error | null>(null);

  const handlePick = useCallback(
    (item: Item): void => {
      try {
        const candidate = item as Item;
        if (!candidate.enabled) {
          throw new Error("disabled");
        }
        onPick?.(candidate);
      } catch (caught: unknown) {
        setError(caught instanceof Error ? caught : new Error("unknown"));
      }
    },
    [onPick],
  );

  const renderItem = (item: Item, index: number): React.ReactNode => {
    switch (item.id) {
      case "primary":
        return <Text>{item.label}</Text>;
      case "secondary":
        return index > 0 ? <Text>{item.label}</Text> : null;
      default:
        return <Text>Other</Text>;
    }
  };

  return (
    <View>
      {error ? <Text>{error.message}</Text> : null}
      {items.slice(0, DEFAULT_LIMIT).map((item, index) => (
        <Button
          key={item.id}
          title={String(renderItem(item, index))}
          onPress={() => handlePick(item)}
        />
      ))}
    </View>
  );
}
