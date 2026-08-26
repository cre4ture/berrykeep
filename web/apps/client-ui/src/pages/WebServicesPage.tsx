import {
  launchClientWebService,
  listClientWebServices,
  type ClientWebService
} from "@ironmesh/api";
import { ironmeshPrimaryColor, PageHeader } from "@ironmesh/ui";
import { Alert, Badge, Button, Card, Group, SimpleGrid, Stack, Text } from "@mantine/core";
import { IconExternalLink, IconRefresh } from "@tabler/icons-react";
import { useCallback, useEffect, useState } from "react";

export function WebServicesPage() {
  const [services, setServices] = useState<ClientWebService[]>([]);
  const [loading, setLoading] = useState(true);
  const [launching, setLaunching] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setServices(await listClientWebServices());
    } catch (nextError) {
      setError(nextError instanceof Error ? nextError.message : String(nextError));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function openService(service: ClientWebService) {
    const launchKey = `${service.nodeId}:${service.id}`;
    const popup = window.open("about:blank", "_blank");
    if (!popup) {
      setError("The browser blocked the service popup. Allow popups and try again.");
      return;
    }
    setLaunching(launchKey);
    setError(null);
    try {
      popup.opener = null;
      popup.document.title = `Opening ${service.name}…`;
      if (!popup.document.body) {
        throw new Error("The browser did not initialize the service popup.");
      }
      popup.document.body.textContent = `Opening ${service.name} through BerryKeep…`;
      const launch = await launchClientWebService(service.nodeId, service.id);
      popup.location.replace(launch.url);
    } catch (nextError) {
      popup.close();
      setError(nextError instanceof Error ? nextError.message : String(nextError));
    } finally {
      setLaunching(null);
    }
  }

  return (
    <Stack gap="lg">
      <PageHeader
        title="Web services"
        description="Open private node-local HTTP and HTTPS applications through the authenticated IronMesh transport. Each service gets an isolated local browser origin; upstream certificates are verified by its home node."
        actions={
          <Button
            variant="default"
            leftSection={<IconRefresh size={16} />}
            loading={loading}
            onClick={() => void refresh()}
          >
            Refresh
          </Button>
        }
      />

      {error ? <Alert color="red" title="Web service request failed">{error}</Alert> : null}

      {!loading && services.length === 0 ? (
        <Alert color="blue" title="No services available">
          An administrator must configure a web service on the node that can reach it and allow
          this device credential.
        </Alert>
      ) : null}

      <SimpleGrid cols={{ base: 1, md: 2, xl: 3 }}>
        {services.map((service) => {
          const launchKey = `${service.nodeId}:${service.id}`;
          return (
            <Card key={launchKey} withBorder radius="md" padding="lg">
              <Stack gap="md" h="100%">
                <Group justify="space-between" align="flex-start">
                  <Stack gap={2}>
                    <Text fw={700}>{service.name}</Text>
                    <Text size="sm" c="dimmed">{service.description || "Private web service"}</Text>
                  </Stack>
                  <Badge color={ironmeshPrimaryColor} variant="light">private</Badge>
                </Group>
                <Text size="xs" c="dimmed" ff="monospace">
                  Node {service.nodeId}
                </Text>
                <Button
                  mt="auto"
                  leftSection={<IconExternalLink size={16} />}
                  loading={launching === launchKey}
                  disabled={launching !== null && launching !== launchKey}
                  onClick={() => void openService(service)}
                >
                  Open in browser
                </Button>
              </Stack>
            </Card>
          );
        })}
      </SimpleGrid>
    </Stack>
  );
}
