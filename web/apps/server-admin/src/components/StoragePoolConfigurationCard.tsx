import {
  getHostStorageVolumes,
  prepareHostStorageDirectory,
  updateStoragePoolConfig,
  validateStoragePoolConfig,
  type StoragePathConfig,
  type StoragePoolConfig
} from "@ironmesh/api";
import { useMutation, useQuery } from "@tanstack/react-query";
import {
  Alert,
  Button,
  Card,
  Code,
  Group,
  List,
  Select,
  Stack,
  Text,
  TextInput,
  Textarea
} from "@mantine/core";
import { useEffect, useMemo, useState } from "react";
import { useAdminAccess } from "../lib/admin-access";
import {
  RecoveryAlert,
  SelectedVolumeDetails,
  StoragePathEditor,
  cloneStoragePoolConfig,
  findVolume,
  firstErrorMessage,
  formatBytes,
  nextStoragePathId,
  recoveryGuidance
} from "./storage-pool-form-support";

type StoragePoolConfigurationCardProps = {
  config: StoragePoolConfig | null;
  configPath: string | null;
  loading: boolean;
};

type Notice =
  | { kind: "validated"; configPath: string }
  | { kind: "saved"; configPath: string }
  | { kind: "staged"; path: string };

export function StoragePoolConfigurationCard({
  config,
  configPath,
  loading
}: StoragePoolConfigurationCardProps) {
  const { adminTokenOverride } = useAdminAccess();
  const normalizedAdminTokenOverride = adminTokenOverride.trim();
  const initialDraft = useMemo(() => (config ? cloneStoragePoolConfig(config) : null), [config]);
  const [draft, setDraft] = useState<StoragePoolConfig | null>(initialDraft);
  const [advancedDraftText, setAdvancedDraftText] = useState("");
  const [selectedVolumePath, setSelectedVolumePath] = useState<string | null>(null);
  const [directoryName, setDirectoryName] = useState("ironmesh-data");
  const [preparedPath, setPreparedPath] = useState<string | null>(null);
  const [newPathId, setNewPathId] = useState("external-storage");
  const [requestError, setRequestError] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);

  useEffect(() => {
    setDraft(initialDraft);
    setAdvancedDraftText(initialDraft ? JSON.stringify(initialDraft, null, 2) : "");
    setPreparedPath(null);
    setRequestError(null);
    setNotice(null);
  }, [initialDraft]);

  const hostVolumesQuery = useQuery({
    queryKey: ["storage-pool", "host-volumes", normalizedAdminTokenOverride],
    queryFn: () => getHostStorageVolumes(normalizedAdminTokenOverride || undefined),
    enabled: Boolean(config) && !loading
  });
  const selectedVolume = findVolume(hostVolumesQuery.data?.volumes, selectedVolumePath);

  const prepareMutation = useMutation({
    mutationFn: () =>
      prepareHostStorageDirectory(
        {
          mount_path: selectedVolumePath ?? "",
          directory_name: directoryName
        },
        normalizedAdminTokenOverride || undefined
      ),
    onMutate: () => setRequestError(null),
    onSuccess: (response) => {
      setPreparedPath(response.path);
      setNewPathId((current) => nextStoragePathId(draft?.paths ?? [], current));
    },
    onError: (error) => setRequestError(firstErrorMessage(error) ?? "Storage preflight failed.")
  });
  const validateMutation = useMutation({
    mutationFn: (next: StoragePoolConfig) =>
      validateStoragePoolConfig(next, normalizedAdminTokenOverride || undefined),
    onMutate: () => setRequestError(null),
    onSuccess: (response) => setNotice({ kind: "validated", configPath: response.config_path }),
    onError: (error) => setRequestError(firstErrorMessage(error) ?? "Validation failed.")
  });
  const saveMutation = useMutation({
    mutationFn: (next: StoragePoolConfig) =>
      updateStoragePoolConfig(next, normalizedAdminTokenOverride || undefined),
    onMutate: () => setRequestError(null),
    onSuccess: (response) => {
      setNotice({ kind: "saved", configPath: response.config_path });
    },
    onError: (error) => setRequestError(firstErrorMessage(error) ?? "Saving the configuration failed.")
  });
  const mutationPending =
    prepareMutation.isPending || validateMutation.isPending || saveMutation.isPending;

  const updatePath = (index: number, changes: Partial<StoragePathConfig>) => {
    setDraft((current) =>
      current
        ? {
            ...current,
            paths: current.paths.map((path, pathIndex) =>
              pathIndex === index ? { ...path, ...changes } : path
            )
          }
        : current
    );
    setNotice(null);
    setRequestError(null);
  };

  const addManualPath = () => {
    setDraft((current) => {
      if (!current) {
        return current;
      }
      const id = nextStoragePathId(current.paths, "storage");
      return {
        ...current,
        paths: [
          ...current.paths,
          {
            id,
            path: "/path/to/ironmesh-data",
            state: "active",
            weight: 1,
            reserve_bytes: 0
          }
        ]
      };
    });
    setNotice(null);
    setRequestError(null);
  };

  const addPreparedPath = () => {
    if (!draft || !preparedPath) {
      return;
    }
    const id = newPathId.trim();
    if (!id) {
      setRequestError("Enter a stable storage-path ID before adding the checked path.");
      return;
    }
    if (draft.paths.some((path) => path.id === id)) {
      setRequestError(`The storage-path ID ${id} is already in use.`);
      return;
    }
    if (draft.paths.some((path) => path.path === preparedPath)) {
      setRequestError("This checked directory is already part of the draft configuration.");
      return;
    }
    setDraft({
      ...draft,
      paths: [
        ...draft.paths,
        {
          id,
          path: preparedPath,
          state: "active",
          weight: 1,
          reserve_bytes: 0
        }
      ]
    });
    setNotice({ kind: "staged", path: preparedPath });
    setPreparedPath(null);
    setRequestError(null);
  };

  const validateDraft = () => {
    if (draft) {
      void validateMutation.mutateAsync(draft);
    }
  };
  const saveDraft = () => {
    if (draft) {
      void saveMutation.mutateAsync(draft);
    }
  };
  const applyAdvancedJson = () => {
    try {
      const next = JSON.parse(advancedDraftText) as StoragePoolConfig;
      setDraft(next);
      setNotice(null);
      setRequestError(null);
    } catch {
      setRequestError("The advanced storage-pool configuration must be valid JSON before it can be applied.");
    }
  };
  const resetDraft = () => {
    setDraft(initialDraft);
    setAdvancedDraftText(initialDraft ? JSON.stringify(initialDraft, null, 2) : "");
    setPreparedPath(null);
    setNotice(null);
    setRequestError(null);
  };

  const recovery = recoveryGuidance(requestError);
  const hostInventoryError = firstErrorMessage(hostVolumesQuery.error);

  return (
    <Card withBorder radius="md" padding="lg">
      <Stack gap="md">
        <Stack gap={4}>
          <Text fw={700}>Storage-pool configuration</Text>
          <Text size="sm" c="dimmed" maw={860}>
            Add and manage node-local chunk and manifest paths without editing JSON. The node data directory still owns
            metadata, TLS material, and setup state.
          </Text>
          <Text size="xs" c="dimmed">
            Configuration file: <Code>{configPath ?? "loading…"}</Code>
          </Text>
        </Stack>

        <Alert color="blue" title="Host operations stay explicit">
          The built-in host storage agent lists mounted volumes and verifies access as the IronMesh service account. It
          never mounts, formats, ejects, or restarts anything. Save a checked path, then restart the IronMesh service
          through the host&apos;s normal service manager when a controlled restart is possible.
        </Alert>

        <Card withBorder radius="sm" padding="md">
          <Stack gap="sm">
            <Stack gap={2}>
              <Text fw={600}>1. Prepare an attached volume</Text>
              <Text size="sm" c="dimmed">
                Select an already mounted volume and an empty or existing child directory. The write check uses the
                same account that runs this node.
              </Text>
            </Stack>
            {hostInventoryError ? (
              <Alert color="red" title="Could not inspect mounted volumes">
                {hostInventoryError}
              </Alert>
            ) : null}
            <Group grow align="flex-end">
              <Select
                label="Mounted volume"
                placeholder={hostVolumesQuery.isLoading ? "Inspecting volumes…" : "Choose a mounted volume"}
                data={(hostVolumesQuery.data?.volumes ?? []).map((volume) => ({
                  value: volume.mount_path,
                  label: `${volume.name || volume.mount_path} — ${volume.mount_path} (${formatBytes(volume.available_bytes)} free)`
                }))}
                value={selectedVolumePath}
                onChange={(value) => {
                  setSelectedVolumePath(value);
                  setPreparedPath(null);
                  setRequestError(null);
                }}
                disabled={loading || !config || mutationPending}
                searchable
                nothingFoundMessage="No mounted volumes reported by the node"
              />
              <TextInput
                label="Storage subfolder"
                description="A single directory name, for example ironmesh-data"
                value={directoryName}
                onChange={(event) => {
                  setDirectoryName(event.currentTarget.value);
                  setPreparedPath(null);
                  setRequestError(null);
                }}
                disabled={loading || !config || mutationPending}
              />
            </Group>
            {selectedVolume ? <SelectedVolumeDetails volume={selectedVolume} /> : null}
            <Group justify="space-between">
              <Button
                variant="subtle"
                onClick={() => void hostVolumesQuery.refetch()}
                loading={hostVolumesQuery.isFetching}
                disabled={loading || mutationPending}
              >
                Refresh volumes
              </Button>
              <Button
                variant="light"
                onClick={() => void prepareMutation.mutateAsync()}
                loading={prepareMutation.isPending}
                disabled={
                  loading ||
                  !config ||
                  !selectedVolumePath ||
                  !directoryName.trim() ||
                  selectedVolume?.read_only ||
                  validateMutation.isPending ||
                  saveMutation.isPending
                }
              >
                Prepare and check storage
              </Button>
            </Group>
            {preparedPath ? (
              <Alert color="green" title="Directory is writable by the node service">
                <Stack gap="xs">
                  <Text size="sm">
                    <Code>{preparedPath}</Code> passed the create, write, sync, and cleanup check.
                  </Text>
                  <Group align="flex-end">
                    <TextInput
                      label="Stable storage-path ID"
                      description="Keep this ID unchanged after the node has stored data there."
                      value={newPathId}
                      onChange={(event) => setNewPathId(event.currentTarget.value)}
                      disabled={mutationPending}
                    />
                    <Button onClick={addPreparedPath} disabled={mutationPending}>
                      Add checked path
                    </Button>
                  </Group>
                </Stack>
              </Alert>
            ) : null}
          </Stack>
        </Card>

        {requestError ? (
          <Alert color="red" title="Storage check or configuration failed" withCloseButton onClose={() => setRequestError(null)}>
            {requestError}
          </Alert>
        ) : null}
        {recovery ? <RecoveryAlert title={recovery.title} steps={recovery.steps} /> : null}
        {notice?.kind === "staged" ? (
          <Alert color="blue" title="Checked path added to the draft" withCloseButton onClose={() => setNotice(null)}>
            <Code>{notice.path}</Code> is not active yet. Validate and save the draft, then restart the service.
          </Alert>
        ) : null}
        {notice?.kind === "validated" ? (
          <Alert color="green" title="Configuration is valid" withCloseButton onClose={() => setNotice(null)}>
            The configuration passed the server-side checks for <Code>{notice.configPath}</Code>. Saving it still
            requires a service restart before the node uses it.
          </Alert>
        ) : null}
        {notice?.kind === "saved" ? (
          <Alert color="yellow" title="Configuration saved — restart required" withCloseButton onClose={() => setNotice(null)}>
            The next IronMesh service start will load <Code>{notice.configPath}</Code>. Restart it through the
            platform&apos;s host administration interface; IronMesh will not initiate that restart itself.
          </Alert>
        ) : null}

        <Stack gap="sm">
          <Group justify="space-between">
            <Stack gap={2}>
              <Text fw={600}>2. Review the storage paths</Text>
              <Text size="sm" c="dimmed">
                Use <strong>draining</strong> before moving data away from a path. Do not remove a path with local data
                until it has been drained, restarted, and rebalanced.
              </Text>
            </Stack>
            <Button variant="light" onClick={addManualPath} disabled={loading || !draft || mutationPending}>
              Add manual path
            </Button>
          </Group>
          {draft?.paths.map((path, index) => (
            <StoragePathEditor
              key={`storage-path-${index}`}
              path={path}
              canRemove={draft.paths.length > 1}
              disabled={loading || mutationPending}
              onChange={(changes) => updatePath(index, changes)}
              onRemove={() => {
                setDraft((current) =>
                  current ? { ...current, paths: current.paths.filter((_, pathIndex) => pathIndex !== index) } : current
                );
                setNotice(null);
                setRequestError(null);
              }}
            />
          ))}
        </Stack>

        <Group justify="flex-end">
          <Button variant="subtle" onClick={resetDraft} disabled={loading || !draft || mutationPending}>
            Reset to running configuration
          </Button>
          <Button
            variant="light"
            onClick={validateDraft}
            loading={validateMutation.isPending}
            disabled={loading || !draft || saveMutation.isPending || prepareMutation.isPending}
          >
            Validate configuration
          </Button>
          <Button
            onClick={saveDraft}
            loading={saveMutation.isPending}
            disabled={loading || !draft || validateMutation.isPending || prepareMutation.isPending}
          >
            Save configuration
          </Button>
        </Group>

        <Card withBorder radius="sm" padding="md">
          <Stack gap="sm">
            <Text fw={600}>If a storage check fails</Text>
            <List size="sm" spacing="xs">
              <List.Item>
                <strong>Volume unavailable:</strong> reconnect or mount it in the operating system, then refresh the
                volume list. Use a stable mount location for removable media.
              </List.Item>
              <List.Item>
                <strong>Read-only or write check failed:</strong> grant the service account access to the mounted volume,
                or select a filesystem that supports the required writes. Run the check again.
              </List.Item>
              <List.Item>
                <strong>Overlapping paths or duplicate IDs:</strong> use separate, non-nested directories and keep every
                path ID unique and stable.
              </List.Item>
              <List.Item>
                <strong>No active storage or rebalance problem:</strong> leave at least one active writable path, ensure
                it has sufficient free capacity, restart after saving, then retry the rebalance.
              </List.Item>
            </List>
          </Stack>
        </Card>

        <details>
          <summary>Advanced JSON configuration</summary>
          <Stack gap="sm" mt="sm">
            <Text size="sm" c="dimmed">
              Use this only for a reviewed configuration export. Apply it to the draft, then run the same server-side
              validation before saving.
            </Text>
            <Textarea
              label="Advanced storage-pool JSON"
              autosize
              minRows={10}
              maxRows={24}
              value={advancedDraftText}
              onChange={(event) => setAdvancedDraftText(event.currentTarget.value)}
              disabled={loading || !draft || mutationPending}
              styles={{ input: { fontFamily: "monospace", fontSize: 12 } }}
            />
            <Group justify="flex-end">
              <Button variant="default" onClick={() => setAdvancedDraftText(JSON.stringify(draft, null, 2))} disabled={!draft || mutationPending}>
                Reset JSON to draft
              </Button>
              <Button variant="light" onClick={applyAdvancedJson} disabled={!draft || mutationPending}>
                Apply JSON to draft
              </Button>
            </Group>
          </Stack>
        </details>
      </Stack>
    </Card>
  );
}
