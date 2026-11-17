import { Alert, Badge, Button, Card, Divider, Group, Stack, Text } from "@mantine/core";
import {
  getClientConnectionServices,
  type ClientConnectionServicesSnapshot,
  type ClientServiceHealth
} from "@berrykeep/api";
import { useEffect, useState } from "react";

const SERVICE_CHECK_INTERVAL_MS = 30_000;

export function ConnectionServicesStatus() {
  const [snapshot, setSnapshot] = useState<ClientConnectionServicesSnapshot | null>(null);
  const [checking, setChecking] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);

  useEffect(() => {
    const controller = new AbortController();
    let running = false;
    async function checkServices() {
      if (running) return;
      running = true;
      setChecking(true);
      try {
        const next = await getClientConnectionServices(controller.signal);
        if (!controller.signal.aborted) {
          setSnapshot(next);
          setError(null);
        }
      } catch (nextError) {
        if (!controller.signal.aborted) {
          setError(nextError instanceof Error ? nextError.message : "Service check failed");
        }
      } finally {
        running = false;
        if (!controller.signal.aborted) setChecking(false);
      }
    }
    void checkServices();
    const interval = window.setInterval(() => void checkServices(), SERVICE_CHECK_INTERVAL_MS);
    return () => {
      controller.abort();
      window.clearInterval(interval);
    };
  }, [refresh]);

  return (
    <Card component="section" aria-label="Rendezvous and iroh relay services" withBorder radius="md" padding="lg">
      <Stack gap="md">
        <Group justify="space-between" align="flex-start">
          <Stack gap={4}>
            <Text fw={700}>Rendezvous and iroh relay services</Text>
            <Text size="sm" c="dimmed">
              Service reachability, independent of connections to storage nodes. Checked every 30 seconds.
            </Text>
          </Stack>
          <Button variant="default" loading={checking} onClick={() => setRefresh((value) => value + 1)}>
            Check services
          </Button>
        </Group>
        {error ? (
          <Alert color="red" title="Service check failed">
            {error}{snapshot ? " Showing previous check results." : ""}
          </Alert>
        ) : null}
        {!snapshot && checking ? <Text size="sm" c="dimmed">Checking services…</Text> : null}
        {snapshot?.endpoints.length === 0 ? (
          <Text size="sm" c="dimmed">No Rendezvous services configured.</Text>
        ) : null}
        {snapshot?.endpoints.map((endpoint) => (
          <Stack key={endpoint.rendezvous.url} gap="sm">
            <Divider />
            <ServiceStatus label="Rendezvous" service={endpoint.rendezvous} />
            {endpoint.iroh_relays.map((relay) => (
              <ServiceStatus key={relay.url} label="iroh relay" service={relay} />
            ))}
            {endpoint.relay_discovery_error ? (
              <Alert color="gray" title="iroh relay status unknown">{endpoint.relay_discovery_error}</Alert>
            ) : endpoint.iroh_relays.length === 0 ? (
              <Text size="sm" c="dimmed">No iroh relay advertised</Text>
            ) : null}
          </Stack>
        ))}
      </Stack>
    </Card>
  );
}

function ServiceStatus({ label, service }: { label: string; service: ClientServiceHealth }) {
  const status = {
    reachable: { label: "Reachable", color: "green" },
    unhealthy: { label: "Unhealthy", color: "orange" },
    unreachable: { label: "Unreachable", color: "red" }
  }[service.status];

  return (
    <Stack gap={4} style={{ minWidth: 0, overflowWrap: "anywhere" }}>
      <Group gap="sm">
        <Text fw={600} size="sm">{label}</Text>
        <Badge color={status.color} variant="light">{status.label}</Badge>
      </Group>
      <Text size="sm">{service.url}</Text>
      <Text size="xs" c="dimmed">
        {service.http_status !== null ? `HTTP ${service.http_status} · ` : ""}
        {service.latency_ms} ms · Checked {new Date(service.checked_at_unix_ms).toLocaleString()}
      </Text>
      {service.reported_status ? <Text size="xs">Reported status: {service.reported_status}</Text> : null}
      {service.software_version ? <Text size="xs">Version {service.software_version}</Text> : null}
      {service.error ? <Text size="sm" c="red">{service.error}</Text> : null}
    </Stack>
  );
}
