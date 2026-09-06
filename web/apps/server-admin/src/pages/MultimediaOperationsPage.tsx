import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  getOperationRunHistory,
  getOperationRunResults,
  getAdminMediaThumbnail,
  getOperations,
  listAdminStoreEntries,
  startOperationRun,
  type GeoApplyItemResult,
  type GeoProposal,
  type GeoProposalChunk,
  type OperationResultChunk,
  type OperationRun,
  type OperationRunStatus
} from "@ironmesh/api";
import { IconFolder, IconPlayerPlay, IconRefresh } from "@tabler/icons-react";
import {
  Alert,
  Badge,
  Button,
  Card,
  Checkbox,
  Group,
  Image,
  Loader,
  NumberInput,
  Select,
  Stack,
  Table,
  Text,
  Title
} from "@mantine/core";
import { useEffect, useMemo, useReducer, useRef, useState } from "react";
import { formatUnixTs } from "../lib/format";
import { useAdminAccess } from "../lib/admin-access";

const PROPOSE_OPERATION_ID = "multimedia.geolocation.propose";
const APPLY_OPERATION_ID = "multimedia.geolocation.apply";
const RESULTS_LIMIT = 10;
const PROPOSALS_PER_REVIEW_PAGE = 25;
const MAX_GEO_TIME_WINDOW_SECONDS = 60 * 60;

type OperationResultsPaginationState = {
  offset: number;
  previousOffsets: number[];
};

type OperationResultsPaginationAction =
  | { type: "next"; nextOffset: number }
  | { type: "previous" }
  | { type: "reset" };

function operationResultsPaginationReducer(
  state: OperationResultsPaginationState,
  action: OperationResultsPaginationAction
): OperationResultsPaginationState {
  switch (action.type) {
    case "next":
      if (action.nextOffset <= state.offset) {
        return state;
      }
      return {
        offset: action.nextOffset,
        previousOffsets: [...state.previousOffsets, state.offset]
      };
    case "previous": {
      const previousOffset = state.previousOffsets[state.previousOffsets.length - 1];
      if (previousOffset === undefined) {
        return state;
      }
      return {
        offset: previousOffset,
        previousOffsets: state.previousOffsets.slice(0, -1)
      };
    }
    case "reset":
      return { offset: 0, previousOffsets: [] };
  }
}

type InferenceSettings = {
  maxAnchorTimeDeltaSeconds: number;
  segmentGapSeconds: number;
  maxAnchorSpeedKmh: number;
};

const defaultSettings: InferenceSettings = {
  maxAnchorTimeDeltaSeconds: 5 * 60,
  segmentGapSeconds: 5 * 60,
  maxAnchorSpeedKmh: 50
};

export function MultimediaOperationsPage() {
  const queryClient = useQueryClient();
  const { adminTokenOverride, sessionLoading, sessionStatus } = useAdminAccess();
  const [browsePrefix, setBrowsePrefix] = useState("");
  const [selectedPrefix, setSelectedPrefix] = useState("");
  const [settings, setSettings] = useState(defaultSettings);
  // `undefined` means no automatic choice has been made yet; `null` is the
  // user's deliberate clear action and must remain clearable.
  const [selectedAnalysisRunId, setSelectedAnalysisRunId] = useState<string | null | undefined>(undefined);
  const [selectedApplyRunId, setSelectedApplyRunId] = useState<string | null | undefined>(undefined);
  const [proposalResultsPagination, dispatchProposalResultsPagination] = useReducer(
    operationResultsPaginationReducer,
    { offset: 0, previousOffsets: [] }
  );
  const [applyResultsPagination, dispatchApplyResultsPagination] = useReducer(
    operationResultsPaginationReducer,
    { offset: 0, previousOffsets: [] }
  );
  const [selectedChunkProposalCounts, setSelectedChunkProposalCounts] = useState<Map<string, number>>(new Map());
  const [selectedProposalChunkIds, setSelectedProposalChunkIds] = useState<Map<string, string>>(new Map());

  const normalizedAdminTokenOverride = adminTokenOverride.trim();
  const hasAdminAccess =
    Boolean(normalizedAdminTokenOverride) || Boolean(sessionStatus?.authenticated);
  const canInspect = !sessionLoading && (!sessionStatus?.login_required || hasAdminAccess);

  const operationsQuery = useQuery({
    queryKey: ["multimedia-operations", "catalog", normalizedAdminTokenOverride],
    queryFn: () => getOperations(normalizedAdminTokenOverride || undefined),
    enabled: canInspect
  });
  const proposalOperationAvailable =
    operationsQuery.data?.operations.some((operation) => operation.id === PROPOSE_OPERATION_ID) ??
    false;
  const proposalHistoryQuery = useQuery({
    queryKey: [
      "multimedia-operations",
      "history",
      PROPOSE_OPERATION_ID,
      normalizedAdminTokenOverride
    ],
    queryFn: () =>
      getOperationRunHistory(
        { operationId: PROPOSE_OPERATION_ID, limit: 100 },
        normalizedAdminTokenOverride || undefined
      ),
    enabled: canInspect,
    refetchInterval: (query) =>
      query.state.data?.runs.some(isUnfinishedRun) ? 3_000 : false
  });
  const applyHistoryQuery = useQuery({
    queryKey: [
      "multimedia-operations",
      "history",
      APPLY_OPERATION_ID,
      normalizedAdminTokenOverride
    ],
    queryFn: () =>
      getOperationRunHistory(
        { operationId: APPLY_OPERATION_ID, limit: 100 },
        normalizedAdminTokenOverride || undefined
      ),
    enabled: canInspect,
    refetchInterval: (query) =>
      query.state.data?.runs.some(isUnfinishedRun) ? 3_000 : false
  });
  const prefixQuery = useQuery({
    queryKey: ["multimedia-operations", "prefix-picker", browsePrefix, normalizedAdminTokenOverride],
    queryFn: () =>
      listAdminStoreEntries(
        browsePrefix || undefined,
        1,
        null,
        normalizedAdminTokenOverride || undefined,
        { view: "tree", limit: 200 }
      ),
    enabled: canInspect
  });

  const proposalRuns = proposalHistoryQuery.data?.runs ?? [];
  const applyRuns = applyHistoryQuery.data?.runs ?? [];
  const proposalSlotOccupied = proposalRuns.some(isUnfinishedRun);
  const applySlotOccupied = applyRuns.some(isUnfinishedRun);
  const runs = [...proposalRuns, ...applyRuns].sort(
    (left, right) => right.created_at_unix - left.created_at_unix
  );
  const activeRunIds = new Set(runs.filter(isUnfinishedRun).map((run) => run.run_id));
  const selectedChunkIds = useMemo(
    () => new Set(selectedChunkProposalCounts.keys()),
    [selectedChunkProposalCounts]
  );
  const selectedProposalIds = useMemo(
    () => new Set(selectedProposalChunkIds.keys()),
    [selectedProposalChunkIds]
  );
  const selectedAnalysisRun =
    proposalRuns.find((run) => run.run_id === selectedAnalysisRunId) ?? null;
  const selectedApplyRun = applyRuns.find((run) => run.run_id === selectedApplyRunId) ?? null;
  const proposalResultsQuery = useQuery({
    queryKey: [
      "multimedia-operations",
      "results",
      selectedAnalysisRun?.run_id ?? null,
      proposalResultsPagination.offset,
      normalizedAdminTokenOverride
    ],
    queryFn: () =>
      getOperationRunResults(
        selectedAnalysisRun!.run_id,
        { limit: RESULTS_LIMIT, offset: proposalResultsPagination.offset },
        normalizedAdminTokenOverride || undefined
      ),
    enabled: canInspect && selectedAnalysisRun !== null,
    refetchInterval:
      selectedAnalysisRun && activeRunIds.has(selectedAnalysisRun.run_id) ? 3_000 : false
  });

  const proposalChunks = useMemo(
    () =>
      (proposalResultsQuery.data?.chunks ?? [])
        .filter((chunk) => chunk.result_type === "multimedia.geolocation.proposal_chunk")
        .map(asGeoProposalChunk)
        .filter((chunk): chunk is GeoProposalChunk => chunk !== null),
    [proposalResultsQuery.data?.chunks]
  );
  const applyResultsQuery = useQuery({
    queryKey: [
      "multimedia-operations",
      "results",
      selectedApplyRun?.run_id ?? null,
      applyResultsPagination.offset,
      normalizedAdminTokenOverride
    ],
    queryFn: () =>
      getOperationRunResults(
        selectedApplyRun!.run_id,
        { limit: RESULTS_LIMIT, offset: applyResultsPagination.offset },
        normalizedAdminTokenOverride || undefined
      ),
    enabled: canInspect && selectedApplyRun !== null,
    refetchInterval:
      selectedApplyRun && isUnfinishedRun(selectedApplyRun) ? 3_000 : false
  });
  const applyItems = useMemo(
    () =>
      (applyResultsQuery.data?.chunks ?? [])
        .filter((chunk) => chunk.result_type === "multimedia.geolocation.apply_item")
        .map(asGeoApplyItemResult)
        .filter((item): item is GeoApplyItemResult => item !== null),
    [applyResultsQuery.data?.chunks]
  );

  useEffect(() => {
    if (selectedAnalysisRunId !== undefined || proposalRuns.length === 0) {
      return;
    }
    setSelectedAnalysisRunId(proposalRuns[0].run_id);
  }, [proposalRuns, selectedAnalysisRunId]);

  useEffect(() => {
    if (selectedApplyRunId !== undefined || applyRuns.length === 0) {
      return;
    }
    setSelectedApplyRunId(applyRuns[0].run_id);
  }, [applyRuns, selectedApplyRunId]);

  useEffect(() => {
    setSelectedChunkProposalCounts(new Map());
    setSelectedProposalChunkIds(new Map());
    dispatchProposalResultsPagination({ type: "reset" });
  }, [selectedAnalysisRunId]);

  useEffect(() => {
    dispatchApplyResultsPagination({ type: "reset" });
  }, [selectedApplyRunId]);

  const refresh = async () => {
    await Promise.all([
      queryClient.invalidateQueries({
        queryKey: ["multimedia-operations", "history"]
      }),
      queryClient.invalidateQueries({ queryKey: ["multimedia-operations", "results"] })
    ]);
  };

  const proposeMutation = useMutation({
    mutationFn: () =>
      startOperationRun(
        PROPOSE_OPERATION_ID,
        {
          prefix: selectedPrefix,
          max_anchor_time_delta_seconds: settings.maxAnchorTimeDeltaSeconds,
          segment_gap_seconds: settings.segmentGapSeconds,
          max_anchor_speed_kmh: settings.maxAnchorSpeedKmh
        },
        normalizedAdminTokenOverride || undefined
      ),
    onSuccess: async ({ run }) => {
      setSelectedAnalysisRunId(run.run_id);
      await refresh();
    }
  });
  const applyMutation = useMutation({
    mutationFn: (approve: boolean) => {
      if (!selectedAnalysisRun) {
        throw new Error("Choose an analysis run first.");
      }
      return startOperationRun(
        APPLY_OPERATION_ID,
        {
          approve,
          analysis_run_id: selectedAnalysisRun.run_id,
          proposal_chunk_ids: [...selectedChunkIds],
          proposal_ids: [...selectedProposalIds]
        },
        normalizedAdminTokenOverride || undefined
      );
    },
    onSuccess: async ({ run }) => {
      setSelectedApplyRunId(run.run_id);
      await refresh();
    }
  });

  const childPrefixes = (prefixQuery.data?.entries ?? []).filter(
    (entry) => entry.entry_type === "prefix" || entry.path.endsWith("/")
  );
  const selectedCount = useMemo(() => {
    const selectedChunkIds = new Set(selectedChunkProposalCounts.keys());
    const wholeChunkProposalCount = [...selectedChunkProposalCounts.values()]
      .reduce((total, count) => total + count, 0);
    const individuallySelectedCount = [...selectedProposalChunkIds.values()]
      .filter((chunkId) => !selectedChunkIds.has(chunkId))
      .length;
    return wholeChunkProposalCount + individuallySelectedCount;
  }, [selectedChunkProposalCounts, selectedProposalChunkIds]);
  const analysisOptions = proposalRuns.map((run) => ({
    value: run.run_id,
    label: `${formatUnixTs(run.created_at_unix)} — ${run.status}`
  }));

  return (
    <Stack gap="lg">
      <div>
        <Title order={1}>Multimedia operations</Title>
        <Text c="dimmed" mt="xs">
          Run bounded, persistent multimedia maintenance work. Location proposals are reviewed
          before a separate job writes XMP sidecars.
        </Text>
      </div>

      <Card withBorder>
        <Stack gap="md">
          <div>
            <Title order={3}>Propose missing locations</Title>
            <Text size="sm" c="dimmed">
              Only the chosen folder prefix is scanned. Empty prefixes can never start a scan.
            </Text>
          </div>
          <FolderPrefixPicker
            browsePrefix={browsePrefix}
            selectedPrefix={selectedPrefix}
            entries={childPrefixes.map((entry) => entry.path)}
            loading={prefixQuery.isLoading}
            onBrowse={setBrowsePrefix}
            onSelect={setSelectedPrefix}
          />
          <Group align="end" grow>
            <NumberInput
              label="Maximum anchor distance (seconds)"
              min={1}
              max={MAX_GEO_TIME_WINDOW_SECONDS}
              value={settings.maxAnchorTimeDeltaSeconds}
              onChange={(value) =>
                setSettings((current) => ({
                  ...current,
                  maxAnchorTimeDeltaSeconds: asPositiveNumber(value, current.maxAnchorTimeDeltaSeconds)
                }))
              }
            />
            <NumberInput
              label="Segment gap (seconds)"
              min={1}
              max={MAX_GEO_TIME_WINDOW_SECONDS}
              value={settings.segmentGapSeconds}
              onChange={(value) =>
                setSettings((current) => ({
                  ...current,
                  segmentGapSeconds: asPositiveNumber(value, current.segmentGapSeconds)
                }))
              }
            />
            <NumberInput
              label="Maximum anchor speed (km/h)"
              min={0.1}
              decimalScale={1}
              value={settings.maxAnchorSpeedKmh}
              onChange={(value) =>
                setSettings((current) => ({
                  ...current,
                  maxAnchorSpeedKmh: asPositiveNumber(value, current.maxAnchorSpeedKmh)
                }))
              }
            />
          </Group>
          <Group justify="space-between">
            <Text size="sm" c={selectedPrefix ? undefined : "dimmed"}>
              {selectedPrefix ? `Scan scope: ${selectedPrefix}` : "Choose a folder prefix to enable the scan."}
            </Text>
            <Button
              leftSection={<IconPlayerPlay size={16} />}
              disabled={!selectedPrefix || !proposalOperationAvailable || proposalSlotOccupied}
              loading={proposeMutation.isPending}
              onClick={() => proposeMutation.mutate()}
            >
              Start analysis
            </Button>
          </Group>
          <MutationError error={proposeMutation.error} />
          <MutationError error={operationsQuery.error} />
        </Stack>
      </Card>

      <Card withBorder>
        <Stack gap="md">
          <Group justify="space-between">
            <div>
              <Title order={3}>Run history</Title>
              <Text size="sm" c="dimmed">
                Queued and running jobs are polled. An interrupted run retains every proposal chunk
                published before a server restart.
              </Text>
            </div>
            <Button
              variant="default"
              leftSection={<IconRefresh size={16} />}
              loading={proposalHistoryQuery.isFetching || applyHistoryQuery.isFetching}
              onClick={() => void refresh()}
            >
              Refresh
            </Button>
          </Group>
          {proposalHistoryQuery.isLoading || applyHistoryQuery.isLoading ? (
            <Group justify="center"><Loader size="sm" /></Group>
          ) : (
            <RunHistoryTable runs={runs} />
          )}
          <MutationError error={proposalHistoryQuery.error} />
          <MutationError error={applyHistoryQuery.error} />
        </Stack>
      </Card>

      <Card withBorder>
        <Stack gap="md">
          <div>
            <Title order={3}>Review location proposals</Title>
            <Text size="sm" c="dimmed">
              Select whole semantic chunks or individual proposals. Confirmation always starts a
              separate, revalidating apply job.
            </Text>
          </div>
          <Select
            label="Analysis run"
            placeholder="No analysis run yet"
            data={analysisOptions}
            value={selectedAnalysisRunId ?? null}
            onChange={setSelectedAnalysisRunId}
            searchable
            clearable
          />
          {selectedAnalysisRun ? (
            <ProposalReview
              chunks={proposalChunks}
              selectedChunkIds={selectedChunkIds}
              selectedProposalIds={selectedProposalIds}
              adminTokenOverride={normalizedAdminTokenOverride || undefined}
              onToggleChunk={(chunkId, proposalCount) => {
                setSelectedChunkProposalCounts((selected) => {
                  const next = new Map(selected);
                  if (next.has(chunkId)) {
                    next.delete(chunkId);
                  } else {
                    next.set(chunkId, proposalCount);
                  }
                  return next;
                });
              }}
              onToggleProposal={(proposalId, chunkId) => {
                setSelectedProposalChunkIds((selected) => {
                  const next = new Map(selected);
                  if (next.has(proposalId)) {
                    next.delete(proposalId);
                  } else {
                    next.set(proposalId, chunkId);
                  }
                  return next;
                });
              }}
            />
          ) : (
            <Alert color="gray">Start or select an analysis run to review persisted proposals.</Alert>
          )}
          {selectedAnalysisRun ? (
            <OperationResultsPagination
              offset={proposalResultsPagination.offset}
              count={proposalResultsQuery.data?.chunks.length ?? 0}
              nextOffset={proposalResultsQuery.data?.next_offset ?? null}
              onPrevious={() => dispatchProposalResultsPagination({ type: "previous" })}
              onNext={(nextOffset) => dispatchProposalResultsPagination({ type: "next", nextOffset })}
            />
          ) : null}
          <Group justify="space-between">
            <Text size="sm" c="dimmed">{selectedCount} proposal{selectedCount === 1 ? "" : "s"} selected</Text>
            <Button
              color="teal"
              disabled={!selectedAnalysisRun || selectedCount === 0 || applySlotOccupied}
              loading={applyMutation.isPending}
              onClick={() => {
                const confirmed = window.confirm(
                  `Apply the selected ${selectedCount} location proposal${selectedCount === 1 ? "" : "s"}? This writes GPS metadata to XMP sidecars and cannot be undone automatically.`
                );
                if (confirmed) {
                  applyMutation.mutate(true);
                }
              }}
            >
              Start apply job
            </Button>
          </Group>
          <MutationError error={applyMutation.error} />
          <MutationError error={proposalResultsQuery.error} />
        </Stack>
      </Card>

      {applyRuns.length > 0 ? (
        <Card withBorder>
          <Stack gap="md">
            <div>
              <Title order={3}>Apply results</Title>
              <Text size="sm" c="dimmed">
                Each selected proposal is recorded separately, including stale, already-geotagged, and capture-time-conflict skips.
              </Text>
            </div>
            <Select
              label="Apply run"
              data={applyRuns.map((run) => ({
                value: run.run_id,
                label: `${formatUnixTs(run.created_at_unix)} — ${run.status}`
              }))}
              value={selectedApplyRunId ?? null}
              onChange={setSelectedApplyRunId}
              searchable
              clearable
            />
            {selectedApplyRun ? (
              <>
                <ApplyResultReview run={selectedApplyRun} items={applyItems} />
                <OperationResultsPagination
                  offset={applyResultsPagination.offset}
                  count={applyResultsQuery.data?.chunks.length ?? 0}
                  nextOffset={applyResultsQuery.data?.next_offset ?? null}
                  onPrevious={() => dispatchApplyResultsPagination({ type: "previous" })}
                  onNext={(nextOffset) => dispatchApplyResultsPagination({ type: "next", nextOffset })}
                />
              </>
            ) : null}
            <MutationError error={applyResultsQuery.error} />
          </Stack>
        </Card>
      ) : null}
    </Stack>
  );
}

function FolderPrefixPicker({
  browsePrefix,
  selectedPrefix,
  entries,
  loading,
  onBrowse,
  onSelect
}: {
  browsePrefix: string;
  selectedPrefix: string;
  entries: string[];
  loading: boolean;
  onBrowse: (prefix: string) => void;
  onSelect: (prefix: string) => void;
}) {
  const normalizedBrowsePrefix = normalizePrefix(browsePrefix);
  const parentPrefix = parentOfPrefix(normalizedBrowsePrefix);
  return (
    <Stack gap="xs">
      <Text size="sm" fw={500}>Folder prefix</Text>
      <Group gap="xs">
        <Button
          variant="default"
          size="xs"
          disabled={!normalizedBrowsePrefix}
          onClick={() => onBrowse(parentPrefix)}
        >
          Up one folder
        </Button>
        <Button
          variant={selectedPrefix === normalizedBrowsePrefix ? "light" : "default"}
          size="xs"
          disabled={!normalizedBrowsePrefix}
          onClick={() => onSelect(normalizedBrowsePrefix)}
        >
          Use current folder
        </Button>
        <Text size="sm" c="dimmed">Browsing: {normalizedBrowsePrefix || "/"}</Text>
      </Group>
      {loading ? <Loader size="sm" /> : null}
      {entries.length > 0 ? (
        <Group gap="xs">
          {entries.map((entry) => (
            <Button
              key={entry}
              size="xs"
              variant={selectedPrefix === normalizePrefix(entry) ? "light" : "subtle"}
              leftSection={<IconFolder size={14} />}
              onClick={() => onBrowse(normalizePrefix(entry))}
            >
              {entry.slice(normalizedBrowsePrefix.length).replace(/\/$/, "") || entry}
            </Button>
          ))}
        </Group>
      ) : (
        <Text size="sm" c="dimmed">No child folders at this level.</Text>
      )}
      <Text size="xs" c="dimmed">
        Browsing folders does not change the scan scope; use “Use current folder” to select it.
      </Text>
    </Stack>
  );
}

function RunHistoryTable({ runs }: { runs: OperationRun[] }) {
  if (runs.length === 0) {
    return <Text c="dimmed">No operation runs have been recorded yet.</Text>;
  }
  return (
    <Table.ScrollContainer minWidth={760}>
      <Table striped highlightOnHover>
        <Table.Thead>
          <Table.Tr>
            <Table.Th>Operation</Table.Th>
            <Table.Th>Status</Table.Th>
            <Table.Th>Progress</Table.Th>
            <Table.Th>Started</Table.Th>
            <Table.Th>Termination</Table.Th>
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {runs.map((run) => (
            <Table.Tr key={run.run_id}>
              <Table.Td>
                <Text size="sm" fw={500}>{run.operation_id}</Text>
                <Text size="xs" c="dimmed">{run.run_id}</Text>
              </Table.Td>
              <Table.Td><RunStatusBadge status={run.status} /></Table.Td>
              <Table.Td>
                <Text size="sm">{run.progress.phase ?? "—"}</Text>
                <Text size="xs" c="dimmed">
                  {run.progress.completed ?? 0}{run.progress.total ? ` / ${run.progress.total}` : ""}
                  {run.progress.message ? ` — ${run.progress.message}` : ""}
                </Text>
              </Table.Td>
              <Table.Td>{formatUnixTs(run.started_at_unix ?? run.created_at_unix)}</Table.Td>
              <Table.Td>{run.termination_reason ?? run.error ?? "—"}</Table.Td>
            </Table.Tr>
          ))}
        </Table.Tbody>
      </Table>
    </Table.ScrollContainer>
  );
}

function ApplyResultReview({ run, items }: { run: OperationRun; items: GeoApplyItemResult[] }) {
  const summary = ["applied", "already_has_gps", "already_has_capture_time", "skipped_stale", "failed"]
    .map((key) => {
      const value = run.summary?.[key];
      return typeof value === "number" ? `${key.replaceAll("_", " ")}: ${value}` : null;
    })
    .filter((value): value is string => value !== null);
  if (items.length === 0) {
    return (
      <Alert color={isUnfinishedRun(run) ? "blue" : "gray"}>
        {isUnfinishedRun(run)
          ? "This apply job has not recorded an item outcome yet."
          : "This apply job did not record any item outcomes."}
      </Alert>
    );
  }
  return (
    <Stack gap="xs">
      {summary.length > 0 ? <Text size="sm">{summary.join(" · ")}</Text> : null}
      <Table.ScrollContainer minWidth={720}>
        <Table striped highlightOnHover>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>File</Table.Th>
              <Table.Th>Outcome</Table.Th>
              <Table.Th>Detail</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {items.map((item) => (
              <Table.Tr key={item.proposal_id}>
                <Table.Td>
                  <Text size="sm">{item.media_path}</Text>
                  <Text size="xs" c="dimmed">{item.proposal_id}</Text>
                </Table.Td>
                <Table.Td><ApplyOutcomeBadge outcome={item.status} /></Table.Td>
                <Table.Td><Text size="sm">{item.detail ?? "—"}</Text></Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      </Table.ScrollContainer>
    </Stack>
  );
}

function OperationResultsPagination({
  offset,
  count,
  nextOffset,
  onPrevious,
  onNext
}: {
  offset: number;
  count: number;
  nextOffset: number | null;
  onPrevious: () => void;
  onNext: (offset: number) => void;
}) {
  if (offset === 0 && nextOffset === null) {
    return null;
  }
  return (
    <Group justify="space-between">
      <Text size="xs" c="dimmed">
        Showing result chunks {count === 0 ? 0 : offset + 1}–{offset + count}.
      </Text>
      <Group gap="xs">
        <Button size="xs" variant="default" disabled={offset === 0} onClick={onPrevious}>
          Previous page
        </Button>
        <Button size="xs" variant="default" disabled={nextOffset === null} onClick={() => {
          if (nextOffset !== null) {
            onNext(nextOffset);
          }
        }}>
          Next page
        </Button>
      </Group>
    </Group>
  );
}

function ProposalReview({
  chunks,
  selectedChunkIds,
  selectedProposalIds,
  adminTokenOverride,
  onToggleChunk,
  onToggleProposal
}: {
  chunks: GeoProposalChunk[];
  selectedChunkIds: Set<string>;
  selectedProposalIds: Set<string>;
  adminTokenOverride?: string;
  onToggleChunk: (chunkId: string, proposalCount: number) => void;
  onToggleProposal: (proposalId: string, chunkId: string) => void;
}) {
  if (chunks.length === 0) {
    return <Alert color="gray">This run has not published any reviewable proposal chunks yet.</Alert>;
  }
  return (
    <Stack gap="md">
      {chunks.map((chunk) => (
        <ProposalChunkReview
          key={`${chunk.id}-page-${chunk.proposal_page ?? 0}`}
          chunk={chunk}
          selectedChunkIds={selectedChunkIds}
          selectedProposalIds={selectedProposalIds}
          adminTokenOverride={adminTokenOverride}
          onToggleChunk={onToggleChunk}
          onToggleProposal={onToggleProposal}
        />
      ))}
    </Stack>
  );
}

function ProposalChunkReview({
  chunk,
  selectedChunkIds,
  selectedProposalIds,
  adminTokenOverride,
  onToggleChunk,
  onToggleProposal
}: {
  chunk: GeoProposalChunk;
  selectedChunkIds: Set<string>;
  selectedProposalIds: Set<string>;
  adminTokenOverride?: string;
  onToggleChunk: (chunkId: string, proposalCount: number) => void;
  onToggleProposal: (proposalId: string, chunkId: string) => void;
}) {
  const [proposalOffset, setProposalOffset] = useState(0);
  const visibleProposals = chunk.proposals.slice(
    proposalOffset,
    proposalOffset + PROPOSALS_PER_REVIEW_PAGE
  );
  const nextProposalOffset = proposalOffset + visibleProposals.length < chunk.proposals.length
    ? proposalOffset + PROPOSALS_PER_REVIEW_PAGE
    : null;
  return (
    <Card withBorder radius="sm" p="sm">
      <Stack gap="xs">
        <Group justify="space-between">
          <Checkbox
            label={`${chunk.folder || "/"} — ${chunk.item_count} media items${
              (chunk.proposal_page_count ?? 1) > 1
                ? ` (proposal page ${(chunk.proposal_page ?? 0) + 1} of ${chunk.proposal_page_count})`
                : ""
            }`}
            checked={selectedChunkIds.has(chunk.id)}
            onChange={() => onToggleChunk(chunk.id, chunk.proposal_count ?? chunk.proposals.length)}
          />
          <Text size="xs" c="dimmed">
            {formatCaptureTime(chunk.time_range_start)} – {formatCaptureTime(chunk.time_range_end)}
          </Text>
        </Group>
        <Table.ScrollContainer minWidth={1_150}>
          <Table striped highlightOnHover verticalSpacing="xs">
            <Table.Thead>
              <Table.Tr>
                <Table.Th>Select</Table.Th>
                <Table.Th>Preview</Table.Th>
                <Table.Th>File / capture time</Table.Th>
                <Table.Th>Proposal</Table.Th>
                <Table.Th>Anchors</Table.Th>
                <Table.Th>Plausibility</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {visibleProposals.map((proposal) => (
                <ProposalRow
                  key={proposal.id}
                  proposal={proposal}
                  checked={selectedProposalIds.has(proposal.id)}
                  adminTokenOverride={adminTokenOverride}
                  onToggle={() => onToggleProposal(proposal.id, chunk.id)}
                />
              ))}
            </Table.Tbody>
          </Table>
        </Table.ScrollContainer>
        {chunk.proposals.length > PROPOSALS_PER_REVIEW_PAGE ? (
          <Group justify="space-between">
            <Text size="xs" c="dimmed">
              Showing proposals {proposalOffset + 1}–{proposalOffset + visibleProposals.length} of {chunk.proposals.length} in this result page.
            </Text>
            <Group gap="xs">
              <Button
                size="xs"
                variant="default"
                disabled={proposalOffset === 0}
                onClick={() => setProposalOffset(Math.max(0, proposalOffset - PROPOSALS_PER_REVIEW_PAGE))}
              >
                Previous proposals
              </Button>
              <Button
                size="xs"
                variant="default"
                disabled={nextProposalOffset === null}
                onClick={() => {
                  if (nextProposalOffset !== null) {
                    setProposalOffset(nextProposalOffset);
                  }
                }}
              >
                Next proposals
              </Button>
            </Group>
          </Group>
        ) : null}
      </Stack>
    </Card>
  );
}

function ProposalRow({
  proposal,
  checked,
  adminTokenOverride,
  onToggle
}: {
  proposal: GeoProposal;
  checked: boolean;
  adminTokenOverride?: string;
  onToggle: () => void;
}) {
  return (
    <Table.Tr>
      <Table.Td><Checkbox aria-label={`Select ${proposal.media_path}`} checked={checked} onChange={onToggle} /></Table.Td>
      <Table.Td>
        <DeferredThumbnail mediaPath={proposal.media_path} adminTokenOverride={adminTokenOverride} />
      </Table.Td>
      <Table.Td>
        <Text size="sm" fw={500}>{proposal.media_path}</Text>
        <Text size="xs" c="dimmed">{formatCaptureTime(proposal.capture_time)}</Text>
        <Text size="xs" c="dimmed">{proposal.capture_time.source.replaceAll("_", " ")}; {proposal.capture_time.basis.replaceAll("_", " ")}</Text>
      </Table.Td>
      <Table.Td>
        <Text size="sm">{formatCoordinate(proposal.proposed)}</Text>
        <Text size="xs" c="dimmed">{proposal.method.replaceAll("_", " ")}</Text>
      </Table.Td>
      <Table.Td>
        <AnchorSummary label="Previous" anchor={proposal.previous_anchor} />
        <AnchorSummary label="Next" anchor={proposal.next_anchor} />
      </Table.Td>
      <Table.Td>
        <Text size="xs">
          {proposal.estimated_anchor_speed_kmh === null || proposal.estimated_anchor_speed_kmh === undefined
            ? "No anchor-speed estimate"
            : `${proposal.estimated_anchor_speed_kmh.toFixed(1)} km/h`}
        </Text>
        {proposal.warnings.length ? (
          <Text size="xs" c="yellow.8">{proposal.warnings.join("; ")}</Text>
        ) : (
          <Text size="xs" c="dimmed">No warnings</Text>
        )}
      </Table.Td>
    </Table.Tr>
  );
}

function DeferredThumbnail({
  mediaPath,
  adminTokenOverride
}: {
  mediaPath: string;
  adminTokenOverride?: string;
}) {
  const targetRef = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  useEffect(() => {
    const target = targetRef.current;
    if (!target || typeof IntersectionObserver === "undefined") {
      setVisible(true);
      return undefined;
    }
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          setVisible(true);
          observer.disconnect();
        }
      },
      { rootMargin: "300px" }
    );
    observer.observe(target);
    return () => observer.disconnect();
  }, []);

  return (
    <div ref={targetRef} style={{ width: 72, height: 54 }}>
      {visible ? (
        <AuthenticatedThumbnail mediaPath={mediaPath} adminTokenOverride={adminTokenOverride} />
      ) : null}
    </div>
  );
}

function AuthenticatedThumbnail({
  mediaPath,
  adminTokenOverride
}: {
  mediaPath: string;
  adminTokenOverride?: string;
}) {
  const [source, setSource] = useState<string | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    let objectUrl: string | null = null;
    let cancelled = false;
    setSource(null);
    void getAdminMediaThumbnail(mediaPath, adminTokenOverride, controller.signal)
      .then((blob) => {
        if (cancelled) {
          return;
        }
        objectUrl = URL.createObjectURL(blob);
        setSource(objectUrl);
      })
      .catch(() => {
        // A missing thumbnail is expected while metadata is still being built.
      });
    return () => {
      cancelled = true;
      controller.abort();
      if (objectUrl) {
        URL.revokeObjectURL(objectUrl);
      }
    };
  }, [adminTokenOverride, mediaPath]);
  return (
    <Image
      src={source ?? undefined}
      alt=""
      w={72}
      h={54}
      fit="cover"
      fallbackSrc="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='72' height='54'/%3E"
    />
  );
}

function AnchorSummary({
  label,
  anchor
}: {
  label: string;
  anchor: GeoProposal["previous_anchor"];
}) {
  if (!anchor) {
    return <Text size="xs" c="dimmed">{label}: —</Text>;
  }
  return (
    <Text size="xs">
      {label}: {anchor.path} ({anchor.distance_seconds}s)
    </Text>
  );
}

function MutationError({ error }: { error: unknown }) {
  if (!error) {
    return null;
  }
  return <Alert color="red">{error instanceof Error ? error.message : String(error)}</Alert>;
}

function RunStatusBadge({ status }: { status: OperationRunStatus }) {
  const color =
    status === "completed"
      ? "teal"
      : status === "failed"
        ? "red"
        : status === "interrupted"
          ? "yellow"
          : "blue";
  return <Badge color={color}>{status}</Badge>;
}

function ApplyOutcomeBadge({ outcome }: { outcome: GeoApplyItemResult["status"] }) {
  const color =
    outcome === "applied"
      ? "teal"
      : outcome === "failed"
        ? "red"
        : outcome === "already-has-gps" || outcome === "already-has-capture-time"
          ? "yellow"
          : "gray";
  return <Badge color={color}>{outcome}</Badge>;
}

function asGeoProposalChunk(chunk: OperationResultChunk): GeoProposalChunk | null {
  const payload = chunk.payload as Partial<GeoProposalChunk>;
  return typeof payload.id === "string" && Array.isArray(payload.proposals)
    ? (payload as GeoProposalChunk)
    : null;
}

function asGeoApplyItemResult(chunk: OperationResultChunk): GeoApplyItemResult | null {
  const payload = chunk.payload as Partial<GeoApplyItemResult>;
  return typeof payload.proposal_id === "string" &&
    typeof payload.media_path === "string" &&
    (payload.status === "applied" ||
      payload.status === "already-has-gps" ||
      payload.status === "already-has-capture-time" ||
      payload.status === "skipped-stale" ||
      payload.status === "failed")
    ? (payload as GeoApplyItemResult)
    : null;
}

function isUnfinishedRun(run: OperationRun): boolean {
  return run.status === "queued" || run.status === "running";
}

function normalizePrefix(prefix: string): string {
  const normalized = prefix.trim().replace(/^\/+|\/+$/g, "");
  return normalized ? `${normalized}/` : "";
}

function parentOfPrefix(prefix: string): string {
  const normalized = normalizePrefix(prefix).replace(/\/$/, "");
  const separator = normalized.lastIndexOf("/");
  return separator < 0 ? "" : `${normalized.slice(0, separator + 1)}`;
}


function asPositiveNumber(value: string | number, fallback: number): number {
  const parsed = typeof value === "number" ? value : Number(value);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function formatCaptureTime(captureTime: { unix: number; basis: string }): string {
  const suffix = captureTime.basis === "floating_local" ? " (local time)" : " (UTC-normalized)";
  return `${formatUnixTs(captureTime.unix)}${suffix}`;
}

function formatCoordinate(coordinate: { latitude: number; longitude: number }): string {
  return `${coordinate.latitude.toFixed(6)}, ${coordinate.longitude.toFixed(6)}`;
}
