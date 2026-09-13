import {
  getHostDependencyReport,
  type HostDependencyReport,
  type HostDependencySeverity,
  type HostDependencyStatus
} from "@berrykeep/api";
import { berrykeepPrimaryColor, StatCard } from "@berrykeep/ui";
import { Alert, Badge, Button, Code, Grid, Group, Stack, Table, Text } from "@mantine/core";
import { useCallback, useEffect, useState } from "react";
import { useAdminAccess } from "../lib/admin-access";
import { formatUnixTs } from "../lib/format";

export function DependenciesPage() {
  const { adminTokenOverride } = useAdminAccess();
  const [report, setReport] = useState<HostDependencyReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const payload = await getHostDependencyReport(adminTokenOverride);
      setReport(payload);
    } catch (refreshError) {
      setError(refreshError instanceof Error ? refreshError.message : String(refreshError));
    } finally {
      setLoading(false);
    }
  }, [adminTokenOverride]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const checks = report?.checks ?? [];
  const attentionChecks = checks.filter((check) => isAttentionSeverity(check.severity));
  const criticalCount = attentionChecks.filter((check) => check.severity === "critical").length;
  const informationalMissingCount = checks.filter(
    (check) => check.status === "missing" && !isAttentionSeverity(check.severity)
  ).length;
  const readyCount = checks.filter((check) => check.status === "ready").length;
  const builtinCount = checks.filter((check) => check.status === "builtin").length;
  const optionalCount = checks.filter((check) => check.status === "optional").length;
  const notApplicableCount = checks.filter((check) => check.status === "not_applicable").length;

  return (
    <Stack gap="lg">
      {error ? <Alert color="red" title="Failed to load host dependency status">{error}</Alert> : null}
      {attentionChecks.length > 0 ? (
        <Alert color={criticalCount > 0 ? "red" : "yellow"} title="Storage mount protection needs attention">
          {attentionChecks.length} mount-protection finding{attentionChecks.length === 1 ? " needs" : "s need"} action.
          Review the affected paths and remedies below before restarting or relying on this node.
        </Alert>
      ) : informationalMissingCount > 0 ? (
        <Alert color="blue" title="Informational host tooling unavailable">
          {informationalMissingCount} optional host tool{informationalMissingCount === 1 ? " is" : "s are"} unavailable.
          These checks describe affected optional features and do not indicate a storage mount-protection warning.
        </Alert>
      ) : report ? (
        <Alert color={berrykeepPrimaryColor} title="Host dependency checks passed">
          No host dependency finding currently needs attention.
        </Alert>
      ) : null}
      {optionalCount > 0 ? (
        <Alert color="blue" title="Optional host administration tooling unavailable">
          Cockpit is not installed on this host. BerryKeep does not require it, but you can install and use Cockpit as a
          separate web interface for service restarts, updates, and host reboots.
        </Alert>
      ) : null}
      <Group justify="space-between" align="flex-start">
        <Text c="dimmed" maw={760}>
          This page checks optional host tools plus storage mount protection for a server node actually managed by systemd.
          For systemd services, it shows the live effective dependency result and a per-path remedy for
          <Code> IRONMESH_DATA_DIR </Code> and active or draining storage-pool paths. Cockpit remains a separate,
          separately authenticated interface for host-level operations; BerryKeep does not restart services or the host itself.
        </Text>
        <Button variant="light" onClick={() => void refresh()} loading={loading}>
          Refresh
        </Button>
      </Group>

      <Grid>
        <Grid.Col span={{ base: 12, md: 6, xl: 2 }}>
          <StatCard
            label="Host OS"
            value={report?.host_os || (loading ? "loading..." : "unknown")}
            hint={`Last checked: ${formatUnixTs(report?.generated_at_unix)}`}
          />
        </Grid.Col>
        <Grid.Col span={{ base: 12, md: 6, xl: 2 }}>
          <StatCard
            label="Needs attention"
            value={loading && !report ? "loading..." : String(attentionChecks.length)}
            hint={attentionChecks.length > 0 ? "Warning or critical findings" : "No actionable host findings"}
          />
        </Grid.Col>
        <Grid.Col span={{ base: 12, md: 6, xl: 2 }}>
          <StatCard
            label="Resolved"
            value={loading && !report ? "loading..." : String(readyCount)}
            hint="External dependencies found on this node"
          />
        </Grid.Col>
        <Grid.Col span={{ base: 12, md: 6, xl: 2 }}>
          <StatCard
            label="Built-in"
            value={loading && !report ? "loading..." : String(builtinCount)}
            hint="Checks that do not need host packages"
          />
        </Grid.Col>
        <Grid.Col span={{ base: 12, md: 6, xl: 2 }}>
          <StatCard
            label="Optional"
            value={loading && !report ? "loading..." : String(optionalCount)}
            hint={optionalCount > 0 ? "Advisory checks unavailable" : "Optional tooling detected"}
          />
        </Grid.Col>
        <Grid.Col span={{ base: 12, md: 6, xl: 2 }}>
          <StatCard
            label="Not applicable"
            value={loading && !report ? "loading..." : String(notApplicableCount)}
            hint="Checks skipped for this startup framework"
          />
        </Grid.Col>
      </Grid>

      <Table.ScrollContainer minWidth={1180}>
        <Table striped highlightOnHover withTableBorder withColumnBorders>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Dependency / finding</Table.Th>
              <Table.Th>Severity</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th>Configured target</Table.Th>
              <Table.Th>Effective dependency</Table.Th>
              <Table.Th>Remedy</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {checks.map((check) => (
              <Table.Tr key={check.id}>
                <Table.Td>
                  <Text fw={600} size="sm">
                    {check.feature}
                  </Text>
                  <Text c="dimmed" size="xs">
                    {check.summary}
                  </Text>
                  <Text c="dimmed" size="xs" mt={4}>
                    {check.detail}
                  </Text>
                </Table.Td>
                <Table.Td>
                  <Badge variant="light" color={dependencySeverityColor(check.severity)}>
                    {dependencySeverityLabel(check.severity)}
                  </Badge>
                </Table.Td>
                <Table.Td>
                  <Badge variant="light" color={dependencyBadgeColor(check.status, check.severity)}>
                    {dependencyBadgeLabel(check.status)}
                  </Badge>
                </Table.Td>
                <Table.Td>
                  {check.configured_path ? <Code>{check.configured_path}</Code> : "—"}
                </Table.Td>
                <Table.Td>
                  <Text size="xs" ff="monospace">
                    {check.resolved_path || "not applicable"}
                  </Text>
                </Table.Td>
                <Table.Td>
                  {check.install_hint ? (
                    <Text c={dependencyRemedyColor(check.severity)} size="xs">
                      {check.install_hint}
                    </Text>
                  ) : (
                    <Text c="dimmed" size="xs">
                      —
                    </Text>
                  )}
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      </Table.ScrollContainer>
    </Stack>
  );
}

function dependencyBadgeColor(status: HostDependencyStatus, severity?: HostDependencySeverity | null): string {
  if (severity === "critical") {
    return "red";
  }
  if (severity === "warning") {
    return "yellow";
  }
  switch (status) {
    case "ready":
      return berrykeepPrimaryColor;
    case "missing":
      return "blue";
    case "builtin":
      return "blue";
    case "optional":
      return "gray";
    case "not_applicable":
      return "gray";
  }
}

function dependencyBadgeLabel(status: HostDependencyStatus): string {
  switch (status) {
    case "ready":
      return "ready";
    case "missing":
      return "missing";
    case "builtin":
      return "built-in";
    case "optional":
      return "optional";
    case "not_applicable":
      return "not applicable";
  }
}

function dependencySeverityColor(severity?: HostDependencySeverity | null): string {
  switch (severity) {
    case "critical":
      return "red";
    case "warning":
      return "yellow";
    case "info":
    case null:
    case undefined:
      return "blue";
  }
}

function dependencySeverityLabel(severity?: HostDependencySeverity | null): string {
  return severity ?? "info";
}

function dependencyRemedyColor(severity?: HostDependencySeverity | null): string {
  return isAttentionSeverity(severity) ? (severity === "critical" ? "red" : "yellow") : "dimmed";
}

function isAttentionSeverity(severity?: HostDependencySeverity | null): boolean {
  return severity === "warning" || severity === "critical";
}
