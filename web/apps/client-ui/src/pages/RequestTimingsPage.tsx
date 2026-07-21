import {
  Alert,
  Badge,
  Button,
  Card,
  Group,
  SimpleGrid,
  Stack,
  Table,
  Text
} from "@mantine/core";
import {
  getClientConnectionRoutes,
  type ClientConnectionAttempt,
  type ClientConnectionRouteEndpointSnapshot,
  type ClientConnectionRouteSnapshot
} from "@ironmesh/api";
import { PageHeader, StatCard } from "@ironmesh/ui";
import { useEffect, useMemo, useState } from "react";

const SNAPSHOT_POLL_MS = 2_000;

type TimedAttempt = {
  endpoint: ClientConnectionRouteEndpointSnapshot;
  attempt: ClientConnectionAttempt;
};

export function RequestTimingsPage() {
  const [routes, setRoutes] = useState<ClientConnectionRouteSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  async function loadSnapshot(
    showLoading: boolean,
    shouldApply: () => boolean = () => true
  ) {
    if (showLoading) {
      setLoading(true);
    }
    try {
      const nextRoutes = await getClientConnectionRoutes();
      if (shouldApply()) {
        setRoutes(nextRoutes);
        setError(null);
      }
    } catch (nextError) {
      if (showLoading && shouldApply()) {
        setError(
          nextError instanceof Error ? nextError.message : "Failed loading request timing data"
        );
      }
    } finally {
      if (showLoading && shouldApply()) {
        setLoading(false);
      }
    }
  }

  useEffect(() => {
    let active = true;
    void loadSnapshot(true, () => active);
    const interval = window.setInterval(() => {
      if (active) {
        void loadSnapshot(false, () => active);
      }
    }, SNAPSHOT_POLL_MS);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, []);

  const attempts = useMemo(() => flattenAttempts(routes), [routes]);
  const successful = attempts.filter(
    ({ attempt }) => attempt.outcome === "success" && attempt.totalDurationUs != null
  );
  const averageTotalUs = average(successful.map(({ attempt }) => attempt.totalDurationUs));
  const averageServerUs = average(
    successful.map(({ attempt }) => attempt.serverProcessingDurationUs)
  );
  const averageTransportUs = average(
    successful.map(({ attempt }) => attempt.transportOverheadUs)
  );
  const averageNetworkUs = average(
    successful.map(({ attempt }) => attempt.networkTransferDurationUs)
  );
  const bestClockSample = successful
    .map(({ attempt }) => attempt)
    .filter(
      (attempt) => attempt.clockOffsetUs != null && attempt.clockUncertaintyUs != null
    )
    .sort((left, right) =>
      (left.clockUncertaintyUs ?? Number.MAX_SAFE_INTEGER) -
      (right.clockUncertaintyUs ?? Number.MAX_SAFE_INTEGER)
    )[0];
  const pool = sumSessionPool(routes);
  const averageConnectUs = pool.connectCount > 0 ? pool.connectDurationUs / pool.connectCount : null;
  const relayEndpointConnects =
    routes?.endpoints
      .filter((endpoint) => endpoint.path_kind === "relay_tunnel")
      .reduce(
        (sum, endpoint) => sum + (endpoint.transport_session_pool?.connect_count ?? 0),
        0
      ) ?? 0;
  const averageRelayPairingUs =
    relayEndpointConnects > 0 ? pool.relayPairingDurationUs / relayEndpointConnects : null;

  return (
    <>
      <PageHeader
        title="Request timings"
        description="Live measurements from real client requests, split into server processing, end-to-end transport, session setup, and relay pairing."
        actions={
          <Button variant="default" loading={loading} onClick={() => void loadSnapshot(true)}>
            Refresh snapshot
          </Button>
        }
      />

      {error ? <Alert color="red">{error}</Alert> : null}
      <Alert color="blue" title="How to read these values">
        Server time is measured on the node with a monotonic clock. Transport overhead is total
        client time minus server time and therefore includes sending, receiving, network queues,
        and relay forwarding. The relay cannot see individual requests inside the inner mTLS
        tunnel; its own exact measurement is limited to tunnel pairing and wait time.
      </Alert>

      <SimpleGrid cols={{ base: 1, sm: 2, xl: 7 }}>
        <StatCard label="Observed requests" value={attempts.length} />
        <StatCard label="Average total" value={formatDuration(averageTotalUs)} />
        <StatCard label="Average server" value={formatDuration(averageServerUs)} />
        <StatCard label="Average transport" value={formatDuration(averageTransportUs)} />
        <StatCard label="Average network / relay" value={formatDuration(averageNetworkUs)} />
        <StatCard label="Average session setup" value={formatDuration(averageConnectUs)} />
        <StatCard label="Average relay pairing" value={formatDuration(averageRelayPairingUs)} />
      </SimpleGrid>

      <SimpleGrid cols={{ base: 1, md: 3 }}>
        <StatCard
          label="Session reuse"
          value={`${pool.reuseCount} reused / ${pool.connectCount} connected`}
          hint={`${pool.resetCount} cached sessions reset`}
        />
        <StatCard
          label="Best clock estimate"
          value={formatClock(bestClockSample)}
          hint="NTP-style offset estimate; uncertainty includes network asymmetry and millisecond timestamp resolution."
        />
        <StatCard
          label="Snapshot"
          value={
            routes ? new Date(routes.generated_at_unix_ms).toLocaleTimeString() : loading ? "Loading..." : "n/a"
          }
        />
      </SimpleGrid>

      <Card withBorder radius="md" padding="lg">
        <Stack gap="sm">
          <Group justify="space-between">
            <Text fw={700}>Recent real requests</Text>
            <Badge variant="light">up to 64 per path</Badge>
          </Group>
          {attempts.length === 0 && !loading ? (
            <Text c="dimmed" size="sm">
              No requests have been observed yet. Open the gallery or another remote view and
              this table will update automatically.
            </Text>
          ) : null}
          <Table.ScrollContainer minWidth={1100}>
            <Table striped highlightOnHover withTableBorder>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Time</Table.Th>
                  <Table.Th>Path</Table.Th>
                  <Table.Th>Request</Table.Th>
                  <Table.Th>Total</Table.Th>
                  <Table.Th>Server</Table.Th>
                  <Table.Th>Transport</Table.Th>
                  <Table.Th>Session setup</Table.Th>
                  <Table.Th>Network / relay</Table.Th>
                  <Table.Th>Bytes sent / received</Table.Th>
                  <Table.Th>Clock offset</Table.Th>
                  <Table.Th>Result</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {attempts.map(({ endpoint, attempt }, index) => (
                  <Table.Tr key={`${endpoint.index}-${attempt.startedUnixMs}-${index}`}>
                    <Table.Td>{new Date(attempt.startedUnixMs).toLocaleTimeString()}</Table.Td>
                    <Table.Td>
                      <Stack gap={2}>
                        <Badge color={endpoint.path_kind === "relay_tunnel" ? "violet" : "blue"} variant="light">
                          {endpoint.path_kind}
                        </Badge>
                        <Text size="xs" c="dimmed">{endpoint.locator}</Text>
                      </Stack>
                    </Table.Td>
                    <Table.Td>
                      <Text size="sm" fw={600}>{attempt.method}</Text>
                      <Text size="xs" c="dimmed">{requestPath(attempt.url)}</Text>
                    </Table.Td>
                    <Table.Td>
                      {attempt.outcome !== "success"
                        ? "failed after "
                        : attempt.responseBodyComplete
                          ? ""
                          : "head "}
                      {formatDuration(attempt.totalDurationUs)}
                    </Table.Td>
                    <Table.Td>{formatDuration(attempt.serverProcessingDurationUs)}</Table.Td>
                    <Table.Td>{formatDuration(attempt.transportOverheadUs)}</Table.Td>
                    <Table.Td>
                      {formatDuration(attempt.sessionSetupDurationUs ?? 0)}
                      {attempt.sessionReused ? " (reused)" : ""}
                      {(attempt.relayPairingDurationUs ?? 0) > 0
                        ? `; relay ${formatDuration(attempt.relayPairingDurationUs)}`
                        : ""}
                    </Table.Td>
                    <Table.Td>{formatDuration(attempt.networkTransferDurationUs)}</Table.Td>
                    <Table.Td>{formatBytes(attempt.requestBytes ?? 0)} / {formatBytes(attempt.responseBytes ?? 0)}</Table.Td>
                    <Table.Td>{formatClock(attempt)}</Table.Td>
                    <Table.Td>
                      <Badge color={attempt.outcome === "success" ? "green" : "red"} variant="light">
                        {attempt.statusCode ?? attempt.outcome}
                      </Badge>
                    </Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </Table.ScrollContainer>
        </Stack>
      </Card>
    </>
  );
}

function flattenAttempts(routes: ClientConnectionRouteSnapshot | null): TimedAttempt[] {
  return (routes?.endpoints ?? [])
    .flatMap((endpoint) =>
      (endpoint.recent_attempts ?? []).map((attempt) => ({ endpoint, attempt }))
    )
    .sort(
      (left, right) =>
        (right.attempt.finishedUnixMs ?? right.attempt.startedUnixMs) -
        (left.attempt.finishedUnixMs ?? left.attempt.startedUnixMs)
    );
}

function average(values: Array<number | null | undefined>): number | null {
  const available = values.filter((value): value is number => value != null && Number.isFinite(value));
  return available.length > 0
    ? available.reduce((sum, value) => sum + value, 0) / available.length
    : null;
}

function sumSessionPool(routes: ClientConnectionRouteSnapshot | null) {
  return (routes?.endpoints ?? []).reduce(
    (sum, endpoint) => ({
      connectCount: sum.connectCount + (endpoint.transport_session_pool?.connect_count ?? 0),
      reuseCount: sum.reuseCount + (endpoint.transport_session_pool?.reuse_count ?? 0),
      resetCount: sum.resetCount + (endpoint.transport_session_pool?.reset_count ?? 0),
      connectDurationUs:
        sum.connectDurationUs + (endpoint.transport_session_pool?.connect_duration_us ?? 0),
      relayPairingDurationUs:
        sum.relayPairingDurationUs +
        (endpoint.transport_session_pool?.relay_pairing_duration_us ?? 0)
    }),
    {
      connectCount: 0,
      reuseCount: 0,
      resetCount: 0,
      connectDurationUs: 0,
      relayPairingDurationUs: 0
    }
  );
}

function formatDuration(valueUs: number | null | undefined): string {
  if (valueUs == null || !Number.isFinite(valueUs)) {
    return "n/a";
  }
  return `${(valueUs / 1_000).toFixed(valueUs < 10_000 ? 2 : 1)} ms`;
}

function formatClock(attempt: ClientConnectionAttempt | undefined): string {
  if (attempt?.clockOffsetUs == null || attempt.clockUncertaintyUs == null) {
    return "n/a";
  }
  const sign = attempt.clockOffsetUs >= 0 ? "+" : "";
  return `${sign}${(attempt.clockOffsetUs / 1_000).toFixed(1)} ± ${(attempt.clockUncertaintyUs / 1_000).toFixed(1)} ms`;
}

function formatBytes(value: number): string {
  if (value < 1_024) {
    return `${value} B`;
  }
  return `${(value / 1_024).toFixed(1)} KiB`;
}

function requestPath(value: string): string {
  try {
    const parsed = new URL(value);
    return `${parsed.pathname}${parsed.search}`;
  } catch {
    return value;
  }
}
