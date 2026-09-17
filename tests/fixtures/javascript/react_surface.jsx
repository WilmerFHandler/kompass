import React, { useEffect, useMemo, useState } from "react";
import { FlatList, Pressable, Text } from "react-native";

const sharedStore = createStore({ ready: true });

export const useVisibleItems = (items, query) => {
  const visible = useMemo(
    () =>
      items
        .filter((item) => item?.visible ?? false)
        .map((item) => ({
          ...item,
          label: item.label?.trim() ?? "Unnamed",
        }))
        .filter((item) => query == null || item.label.includes(query)),
    [items, query],
  );

  useEffect(() => {
    if (!visible.length) {
      return;
    }
    notify?.(visible[0]);
  }, [visible]);

  return visible;
};

export default function ListScreen({ items, onSelect }) {
  const [selected, setSelected] = useState(null);
  const visible = useVisibleItems(items, selected?.query ?? null);

  const renderItem = ({ item, index }) => (
    <Pressable
      accessibilityLabel={item?.label ?? `Item ${index}`}
      onPress={() => {
        setSelected(item);
        onSelect?.(item);
      }}
    >
      {item.active ? <Text>{item.label}</Text> : <Text>Hidden</Text>}
    </Pressable>
  );

  return visible.length > 0 ? (
    <FlatList
      data={visible}
      renderItem={renderItem}
      keyExtractor={(item) => String(item.id)}
    />
  ) : (
    <Text>No items</Text>
  );
}
