import type { HostStorageVolume, StoragePathConfig, StoragePathState, StoragePoolConfig } from "@ironmesh/api";
import { Alert, Badge, Button, Card, Group, List, Select, Stack, Text, TextInput } from "@mantine/core";

const GIBIBYTE = 1024 ** 3;

export function StoragePathEditor({
  path,
  canRemove,
  disabled,
  onChange,
  onRemove
}: {
  path: StoragePathConfig;
  canRemove: boolean;
  disabled: boolean;
  onChange: (changes: Partial<StoragePathConfig>) => void;
  onRemove: () => void;
}) {
  return (
    <Card withBorder radius="sm" padding="md">
      <Stack gap="sm">
        <Group justify="space-between">
          <Text fw={600}>{path.id || "New storage path"}</Text>
          <Button color="red" variant="subtle" size="xs" onClick={onRemove} disabled={!canRemove || disabled}>
            Remove path
          </Button>
        </Group>
        <Group grow align="flex-end">
          <TextInput
            label="Path ID"
            description="Stable after first use; letters, digits, - and _ only"
            value={path.id}
            onChange={(event) => onChange({ id: event.currentTarget.value })}
            disabled={disabled}
          />
          <Select
            label="Lifecycle state"
            value={path.state}
            data={[
              { value: "active", label: "Active — accepts new data" },
              { value: "draining", label: "Draining — move data away after restart" },
              { value: "disabled", label: "Disabled — retain data, accept none" }
            ]}
            onChange={(value) => {
              if (isStoragePathState(value)) {
                onChange({ state: value });
              }
            }}
            disabled={disabled}
          />
        </Group>
        <TextInput
          label="Filesystem path"
          description="Must already be a directory when the configuration is validated."
          value={path.path}
          onChange={(event) => onChange({ path: event.currentTarget.value })}
          disabled={disabled}
        />
        <Group grow>
          <TextInput
            label="Placement weight"
            description="Relative preference for new data; must be at least 1."
            inputMode="numeric"
            value={String(path.weight)}
            onChange={(event) => onChange({ weight: parseWholeNumber(event.currentTarget.value, 1) })}
            disabled={disabled}
          />
          <TextInput
            label="Reserved space (GiB)"
            description="Capacity to keep free on this path."
            inputMode="decimal"
            value={formatReserveGiB(path.reserve_bytes)}
            onChange={(event) => onChange({ reserve_bytes: parseReserveGiB(event.currentTarget.value) })}
            disabled={disabled}
          />
        </Group>
      </Stack>
    </Card>
  );
}

export function SelectedVolumeDetails({ volume }: { volume: HostStorageVolume }) {
  return (
    <Group gap="xs">
      <Badge variant="light" color={volume.read_only ? "red" : "green"}>
        {volume.read_only ? "read-only" : "writable candidate"}
      </Badge>
      <Text size="xs" c="dimmed">
        {volume.file_system || "unknown filesystem"} · {formatBytes(volume.available_bytes)} free of {formatBytes(volume.total_bytes)}
        {volume.removable ? " · removable" : ""}
      </Text>
    </Group>
  );
}

export function RecoveryAlert({ title, steps }: { title: string; steps: string[] }) {
  return (
    <Alert color="yellow" title={title}>
      <List size="sm" spacing="xs">
        {steps.map((step) => <List.Item key={step}>{step}</List.Item>)}
      </List>
    </Alert>
  );
}

export function recoveryGuidance(message: string | null): { title: string; steps: string[] } | null {
  if (!message) {
    return null;
  }
  const normalized = message.toLowerCase();
  if (normalized.includes("no longer mounted") || normalized.includes("mounted volume")) {
    return {
      title: "The selected volume is unavailable",
      steps: [
        "Reconnect or mount the volume with the operating system's normal tools.",
        "Refresh the volume list, select the mounted volume again, and rerun the check.",
        "For removable storage, use a stable mount location before saving the node configuration."
      ]
    };
  }
  if (normalized.includes("read-only") || normalized.includes("service account cannot") || normalized.includes("write")) {
    return {
      title: "The node cannot write to this directory",
      steps: [
        "Check that the volume is mounted read-write and that the IronMesh service account can create files there.",
        "Do not change the node service identity just for this check; grant that existing identity access instead.",
        "Run the prepare-and-check step again after correcting the host permission or filesystem issue."
      ]
    };
  }
  if (normalized.includes("overlap") || normalized.includes("duplicate path id")) {
    return {
      title: "The storage paths are not independent",
      steps: [
        "Assign a unique, stable ID to every storage path.",
        "Choose separate directories; neither path may be inside the other.",
        "Validate again before saving so the server checks the final host paths."
      ]
    };
  }
  if (normalized.includes("directory") || normalized.includes("path")) {
    return {
      title: "The storage directory needs attention",
      steps: [
        "Use the mounted-volume picker to prepare a safe child directory, or create the directory through normal host administration.",
        "Use an absolute path and keep the directory dedicated to this node storage path.",
        "Rerun validation after the directory exists and is writable."
      ]
    };
  }
  return {
    title: "Review before retrying",
    steps: [
      "Read the error above and keep the affected path out of the saved configuration until the host issue is resolved.",
      "Ensure at least one active writable path remains available to the node.",
      "After a successful save, restart through the operating system's service manager so the node reloads the configuration."
    ]
  };
}

export function cloneStoragePoolConfig(config: StoragePoolConfig): StoragePoolConfig {
  return {
    version: config.version,
    paths: config.paths.map((path) => ({ ...path }))
  };
}

export function findVolume(
  volumes: HostStorageVolume[] | undefined,
  mountPath: string | null
): HostStorageVolume | null {
  return volumes?.find((volume) => volume.mount_path === mountPath) ?? null;
}

export function nextStoragePathId(paths: StoragePathConfig[], preferred: string): string {
  const normalized = preferred
    .trim()
    .replace(/[^A-Za-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "") || "storage";
  if (!paths.some((path) => path.id === normalized)) {
    return normalized;
  }
  let suffix = 2;
  while (paths.some((path) => path.id === `${normalized}-${suffix}`)) {
    suffix += 1;
  }
  return `${normalized}-${suffix}`;
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) {
    return "unknown";
  }
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value >= 10 || unitIndex === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[unitIndex]}`;
}

export function firstErrorMessage(error: unknown): string | null {
  return error instanceof Error && error.message.trim() ? error.message : null;
}

function isStoragePathState(value: string | null): value is StoragePathState {
  return value === "active" || value === "draining" || value === "disabled";
}

function parseWholeNumber(value: string, fallback: number): number {
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) && parsed >= 0 ? parsed : fallback;
}

function parseReserveGiB(value: string): number {
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return 0;
  }
  return Math.round(parsed * GIBIBYTE);
}

function formatReserveGiB(bytes: number): string {
  const value = bytes / GIBIBYTE;
  return Number.isInteger(value) ? String(value) : value.toFixed(2).replace(/0+$/, "").replace(/\.$/, "");
}
