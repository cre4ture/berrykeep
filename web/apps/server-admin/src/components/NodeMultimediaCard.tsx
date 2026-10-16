import { getNodeMultimedia, updateNodeMultimedia } from "@berrykeep/api";
import { Alert, Badge, Button, Card, Group, Stack, Switch, Text } from "@mantine/core";
import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";

export function NodeMultimediaCard({
  canConfigure,
  adminTokenOverride
}: {
  canConfigure: boolean;
  adminTokenOverride: string;
}) {
  const query = useQuery({
    queryKey: ["hardware-page", "node-multimedia", adminTokenOverride],
    queryFn: () => getNodeMultimedia(adminTokenOverride || undefined),
    enabled: canConfigure
  });
  const [enabled, setEnabled] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (query.data) setEnabled(query.data.enabled);
  }, [query.data]);

  async function save() {
    setSaving(true);
    setError(null);
    try {
      const result = await updateNodeMultimedia(enabled, adminTokenOverride || undefined);
      setEnabled(result.enabled);
      await query.refetch();
    } catch (error) {
      setError(error instanceof Error ? error.message : String(error));
    } finally {
      setSaving(false);
    }
  }

  return (
    <Card withBorder radius="md" padding="lg">
      <Stack gap="sm">
        <Group justify="space-between" align="flex-start">
          <div>
            <Text fw={700}>Multimedia functionality</Text>
            <Text c="dimmed" size="sm" maw={760}>
              Disable thumbnails, media metadata extraction, map views, and multimedia operations
              on hosts with limited RAM or CPU. Storage, replication, and automatic repair continue.
              Use a multimedia-enabled node for these convenience features.
            </Text>
          </div>
          <Badge variant="light">{query.data?.persisted ? "Persisted" : "Runtime only"}</Badge>
        </Group>
        {error || query.error ? (
          <Alert color="red" title="Multimedia configuration unavailable">
            {error ?? (query.error instanceof Error ? query.error.message : String(query.error))}
          </Alert>
        ) : null}
        <Switch
          label="Enable multimedia functionality"
          checked={enabled}
          onChange={(event) => setEnabled(event.currentTarget.checked)}
          disabled={!canConfigure || !query.data || query.isError || saving}
        />
        <Group>
          <Button onClick={() => void save()} loading={saving}
            disabled={!canConfigure || !query.data || query.isError || enabled === query.data.enabled}>
            Save multimedia settings
          </Button>
        </Group>
        <Text size="xs" c="dimmed">
          Independent of client connection priority. Changes apply to new work; active work may finish.
          {query.data?.persistence_source === "runtime_only"
            ? " Configure the startup setting to retain this choice after a restart."
            : " This choice is saved in the node enrollment configuration."}
        </Text>
      </Stack>
    </Card>
  );
}
