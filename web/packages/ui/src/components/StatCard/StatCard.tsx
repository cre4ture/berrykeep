import { Card, Group, Stack, Text, Title } from "@mantine/core";
import type { ReactNode } from "react";

type StatCardProps = {
  label: string;
  value: ReactNode;
  hint?: string;
  testId?: string;
  variant?: "default" | "compact";
};

export function StatCard({ label, value, hint, testId, variant = "default" }: StatCardProps) {
  if (variant === "compact") {
    return (
      <Card
        withBorder
        radius="md"
        padding="sm"
        data-testid={testId}
        data-stat-card-variant={variant}
      >
        <Stack gap={2}>
          <Group justify="space-between" align="flex-start" gap="sm" wrap="nowrap">
            <Text size="xs" tt="uppercase" fw={700} c="dimmed">
              {label}
            </Text>
            <Text size="lg" fw={700} lh={1.1} ta="right">
              {value}
            </Text>
          </Group>
          {hint ? (
            <Text size="xs" c="dimmed" lh={1.25}>
              {hint}
            </Text>
          ) : null}
        </Stack>
      </Card>
    );
  }

  return (
    <Card
      withBorder
      radius="md"
      padding="lg"
      data-testid={testId}
      data-stat-card-variant={variant}
    >
      <Stack gap={6}>
        <Text size="sm" tt="uppercase" fw={700} c="dimmed">
          {label}
        </Text>
        <Title order={3}>{value}</Title>
        {hint ? (
          <Text size="sm" c="dimmed">
            {hint}
          </Text>
        ) : null}
      </Stack>
    </Card>
  );
}
