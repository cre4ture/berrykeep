import { useQuery } from "@tanstack/react-query";
import {
  Alert,
  Badge,
  Box,
  Button,
  Card,
  Container,
  Grid,
  Group,
  Loader,
  SimpleGrid,
  Stack,
  Table,
  Text,
  Title
} from "@mantine/core";
import { IconRefresh, IconShieldCheck } from "@tabler/icons-react";
import {
  ColorSchemeControl,
  IronmeshBrand,
  PageHeader,
  StatCard
} from "@ironmesh/ui/fleet-telemetry";
import {
  Bar,
  BarChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis
} from "recharts";
import { getFleetDashboard, type FleetDashboard } from "./lib/fleet-dashboard";

const PUBLIC_DASHBOARD_REFRESH_MS = 5 * 60 * 1000;

export function App() {
  const dashboardQuery = useQuery({
    queryKey: ["fleet-telemetry", "dashboard"],
    queryFn: getFleetDashboard,
    staleTime: PUBLIC_DASHBOARD_REFRESH_MS,
    refetchInterval: PUBLIC_DASHBOARD_REFRESH_MS
  });
  const dashboard = dashboardQuery.data ?? null;

  return (
    <Box className="fleet-page">
      <Box component="header" className="fleet-header">
        <Container size="lg" className="fleet-header-inner">
          <IronmeshBrand surfaceLabel="Fleet reliability" />
          <Group gap="xs">
            <Badge color="brand" variant="light" leftSection={<IconShieldCheck size={14} />}>
              Public aggregates
            </Badge>
            <ColorSchemeControl />
          </Group>
        </Container>
      </Box>

      <Container component="main" size="lg" className="fleet-content">
        <Stack gap="xl">
          <PageHeader
            title="Fleet reliability"
            description="Anonymous hardware-reliability telemetry, published only as privacy-preserving fleet aggregates."
            actions={
              <Button
                variant="default"
                size="sm"
                leftSection={<IconRefresh size={16} />}
                onClick={() => void dashboardQuery.refetch()}
                loading={dashboardQuery.isFetching}
              >
                Refresh
              </Button>
            }
          />

          {dashboardQuery.isLoading ? <LoadingDashboard /> : null}
          {dashboardQuery.isError ? (
            <Alert color="red" title="Fleet statistics are unavailable">
              {dashboardQuery.error instanceof Error
                ? dashboardQuery.error.message
                : "The public telemetry service did not return a usable dashboard response."}
            </Alert>
          ) : null}
          {dashboard ? <FleetDashboardContent dashboard={dashboard} /> : null}
        </Stack>
      </Container>
    </Box>
  );
}

function LoadingDashboard() {
  return (
    <Card withBorder radius="md" padding="xl">
      <Group justify="center" gap="sm">
        <Loader size="sm" />
        <Text c="dimmed">Loading privacy-preserving fleet statistics…</Text>
      </Group>
    </Card>
  );
}

function FleetDashboardContent({ dashboard }: { dashboard: FleetDashboard }) {
  const countryChartData = dashboard.by_country.map((entry) => ({
    country: entry.country_code,
    participants: entry.subject_count
  }));
  const profileChartData = dashboard.by_hardware_profile.map((entry) => ({
    profile: shortProfileId(entry.hardware_profile_id),
    participants: entry.subject_count
  }));
  const publishedCrossSections =
    dashboard.by_country.length + dashboard.by_hardware_profile.length;

  return (
    <Stack gap="lg">
      <Alert color="brand" title="Privacy-preserving by design" icon={<IconShieldCheck size={18} />}>
        No raw telemetry, IP addresses, node IDs, or telemetry subject IDs are shown here. Country
        and hardware-profile groups appear only when at least {dashboard.k_anonymity_min} distinct
        participants contribute to them.
      </Alert>

      <SimpleGrid cols={{ base: 1, sm: 2, lg: 4 }}>
        <StatCard
          label="Participants recorded"
          value={dashboard.total_subjects.toLocaleString()}
          hint="Distinct pseudonymous telemetry subjects"
        />
        <StatCard
          label="Published countries"
          value={dashboard.by_country.length.toLocaleString()}
          hint={`Groups meeting k = ${dashboard.k_anonymity_min}`}
        />
        <StatCard
          label="Published hardware profiles"
          value={dashboard.by_hardware_profile.length.toLocaleString()}
          hint={`Groups meeting k = ${dashboard.k_anonymity_min}`}
        />
        <StatCard
          label="Last calculated"
          value={formatTimestamp(dashboard.generated_at_unix)}
          hint={`Collector ${dashboard.software_version}`}
        />
      </SimpleGrid>

      <Grid>
        <Grid.Col span={{ base: 12, md: 6 }}>
          <DistributionCard
            title="Participation by country"
            description="Country is derived by the collector from the request source and is never sent by a node."
            emptyMessage="No country groups meet the publication threshold yet."
            data={countryChartData}
            categoryKey="country"
            categoryLabel="Country"
          />
        </Grid.Col>
        <Grid.Col span={{ base: 12, md: 6 }}>
          <DistributionCard
            title="Participation by hardware profile"
            description="Hardware-profile identifiers are stable pseudonymous fingerprints, not vendor or serial details."
            emptyMessage="No hardware-profile groups meet the publication threshold yet."
            data={profileChartData}
            categoryKey="profile"
            categoryLabel="Hardware profile"
          />
        </Grid.Col>
      </Grid>

      <Card withBorder radius="md" padding="lg">
        <Stack gap="md">
          <Group justify="space-between" align="flex-start">
            <Box>
              <Title order={2} size="h3">
                Published aggregate groups
              </Title>
              <Text size="sm" c="dimmed">
                This public view contains {publishedCrossSections.toLocaleString()} country and hardware-profile groups.
              </Text>
            </Box>
            <Badge variant="light">Schema v{dashboard.schema_version}</Badge>
          </Group>
          <Table.ScrollContainer minWidth={460}>
            <Table striped highlightOnHover withTableBorder>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Dimension</Table.Th>
                  <Table.Th>Published group</Table.Th>
                  <Table.Th ta="right">Participants</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {dashboard.by_country.map((entry) => (
                  <Table.Tr key={`country-${entry.country_code}`}>
                    <Table.Td>Country</Table.Td>
                    <Table.Td>{entry.country_code}</Table.Td>
                    <Table.Td ta="right">{entry.subject_count.toLocaleString()}</Table.Td>
                  </Table.Tr>
                ))}
                {dashboard.by_hardware_profile.map((entry) => (
                  <Table.Tr key={`profile-${entry.hardware_profile_id}`}>
                    <Table.Td>Hardware profile</Table.Td>
                    <Table.Td title={entry.hardware_profile_id}>{shortProfileId(entry.hardware_profile_id)}</Table.Td>
                    <Table.Td ta="right">{entry.subject_count.toLocaleString()}</Table.Td>
                  </Table.Tr>
                ))}
                {publishedCrossSections === 0 ? (
                  <Table.Tr>
                    <Table.Td colSpan={3}>
                      <Text c="dimmed" ta="center">
                        No aggregate groups meet the publication threshold yet.
                      </Text>
                    </Table.Td>
                  </Table.Tr>
                ) : null}
              </Table.Tbody>
            </Table>
          </Table.ScrollContainer>
        </Stack>
      </Card>
    </Stack>
  );
}

type DistributionCardProps = {
  title: string;
  description: string;
  emptyMessage: string;
  data: Array<Record<string, string | number>>;
  categoryKey: string;
  categoryLabel: string;
};

function DistributionCard({
  title,
  description,
  emptyMessage,
  data,
  categoryKey,
  categoryLabel
}: DistributionCardProps) {
  return (
    <Card withBorder radius="md" padding="lg" h="100%">
      <Stack gap="sm" h="100%">
        <Box>
          <Title order={2} size="h3">
            {title}
          </Title>
          <Text size="sm" c="dimmed">
            {description}
          </Text>
        </Box>
        {data.length === 0 ? (
          <Text c="dimmed" py="xl" ta="center">
            {emptyMessage}
          </Text>
        ) : (
          <Box h={Math.max(240, data.length * 48)}>
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={data} layout="vertical" margin={{ top: 6, right: 12, bottom: 6, left: 12 }}>
                <CartesianGrid horizontal={false} strokeDasharray="3 3" />
                <XAxis type="number" allowDecimals={false} />
                <YAxis
                  type="category"
                  dataKey={categoryKey}
                  width={categoryKey === "profile" ? 110 : 58}
                  tickLine={false}
                  axisLine={false}
                />
                <Tooltip
                  cursor={{ fill: "rgba(20, 184, 166, 0.12)" }}
                  labelFormatter={(label) => `${categoryLabel}: ${label}`}
                />
                <Bar
                  dataKey="participants"
                  name="Participants"
                  fill="#14b8a6"
                  radius={[0, 6, 6, 0]}
                />
              </BarChart>
            </ResponsiveContainer>
          </Box>
        )}
      </Stack>
    </Card>
  );
}

function shortProfileId(value: string): string {
  const normalized = value.trim();
  if (normalized.length <= 18) {
    return normalized;
  }
  return `${normalized.slice(0, 10)}…${normalized.slice(-6)}`;
}

function formatTimestamp(unixTimestamp: number): string {
  if (!Number.isFinite(unixTimestamp) || unixTimestamp <= 0) {
    return "Unknown";
  }
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short"
  }).format(new Date(unixTimestamp * 1000));
}
