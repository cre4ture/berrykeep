import {
  launchClientWebService,
  listClientWebServiceNodes,
  listClientWebServices,
  listClientWebServicesOnNode,
  type ClientWebServiceNodeResponse,
  type ClientWebService
} from "@ironmesh/api";
import { ironmeshPrimaryColor, PageHeader } from "@ironmesh/ui";
import { Alert, Badge, Button, Card, Code, Group, SimpleGrid, Stack, Text } from "@mantine/core";
import { IconExternalLink, IconRefresh } from "@tabler/icons-react";
import { useCallback, useEffect, useRef, useState } from "react";

type WebServiceNodeState = {
  nodeId: string;
  state: "pending" | "available" | "unavailable";
  services: ClientWebService[];
};

type WebServiceOpenTarget = "in-app" | "browser";

const MAX_CONCURRENT_NODE_SERVICE_REQUESTS = 4;

function sortServices(services: ClientWebService[]): ClientWebService[] {
  return [...services].sort(
    (left, right) =>
      left.name.localeCompare(right.name) ||
      left.nodeId.localeCompare(right.nodeId) ||
      left.id.localeCompare(right.id)
  );
}

export function WebServicesPage() {
  const [directServices, setDirectServices] = useState<ClientWebService[] | null>(null);
  const [nodeStates, setNodeStates] = useState<WebServiceNodeState[]>([]);
  const [loading, setLoading] = useState(true);
  const [launching, setLaunching] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const refreshSequence = useRef(0);
  const refreshController = useRef<AbortController | null>(null);
  const embeddedAndroidClient =
    new URLSearchParams(window.location.search).get("embedded_client") === "android";

  const services = sortServices(
    directServices ?? nodeStates.flatMap((nodeState) => nodeState.services)
  );
  const checkedNodeCount = nodeStates.filter((nodeState) => nodeState.state !== "pending").length;
  const availableNodeCount = nodeStates.filter((nodeState) => nodeState.state === "available").length;
  const unavailableNodeCount = nodeStates.filter((nodeState) => nodeState.state === "unavailable").length;
  const pendingNodeCount = nodeStates.length - checkedNodeCount;

  const refresh = useCallback(async () => {
    const sequence = ++refreshSequence.current;
    refreshController.current?.abort();
    const controller = new AbortController();
    refreshController.current = controller;
    setLoading(true);
    setError(null);
    setDirectServices(null);
    setNodeStates([]);
    try {
      let nodeIds: string[];
      try {
        ({ nodeIds } = await listClientWebServiceNodes({ signal: controller.signal }));
      } catch {
        if (controller.signal.aborted) return;
        const services = await listClientWebServices({ signal: controller.signal });
        if (sequence !== refreshSequence.current || controller.signal.aborted) return;
        setDirectServices(services);
        return;
      }
      if (sequence !== refreshSequence.current) return;

      if (nodeIds.length === 0) {
        const services = await listClientWebServices({ signal: controller.signal });
        if (sequence !== refreshSequence.current || controller.signal.aborted) return;
        setDirectServices(services);
        return;
      }

      setNodeStates(
        nodeIds.map((nodeId) => ({ nodeId, state: "pending", services: [] }))
      );
      let nextNodeIndex = 0;
      await Promise.all(
        Array.from(
          { length: Math.min(MAX_CONCURRENT_NODE_SERVICE_REQUESTS, nodeIds.length) },
          async () => {
            while (!controller.signal.aborted) {
              const nodeId = nodeIds[nextNodeIndex++];
              if (!nodeId) return;
              let response: ClientWebServiceNodeResponse;
              try {
                response = await listClientWebServicesOnNode(nodeId, {
                  signal: controller.signal
                });
              } catch {
                if (controller.signal.aborted) return;
                response = { nodeId, available: false, services: [] };
              }
              if (sequence !== refreshSequence.current || controller.signal.aborted) return;
              setNodeStates((current) =>
                current.map((nodeState) =>
                  nodeState.nodeId === nodeId
                    ? {
                        nodeId,
                        state: response.available ? "available" : "unavailable",
                        services: response.services
                      }
                    : nodeState
                )
              );
            }
          }
        )
      );
    } catch (nextError) {
      if (sequence === refreshSequence.current && !controller.signal.aborted) {
        setError(nextError instanceof Error ? nextError.message : String(nextError));
      }
    } finally {
      if (sequence === refreshSequence.current) {
        setLoading(false);
        if (refreshController.current === controller) refreshController.current = null;
      }
    }
  }, []);

  useEffect(() => {
    void refresh();
    return () => refreshController.current?.abort();
  }, [refresh]);

  async function openService(service: ClientWebService, target: WebServiceOpenTarget = "browser") {
    const launchKey = `${service.nodeId}:${service.id}`;
    let popup: Window | null = null;
    setLaunching(launchKey);
    setError(null);
    try {
      if (embeddedAndroidClient) {
        const launch = await launchClientWebService(service.nodeId, service.id);
        const launchUrl = new URL(launch.url);
        launchUrl.searchParams.set("ironmesh_open", target);
        window.location.assign(launchUrl.toString());
        return;
      }

      popup = window.open("about:blank", "_blank");
      if (!popup) {
        throw new Error("The browser blocked the service popup. Allow popups and try again.");
      }
      popup.opener = null;
      popup.document.title = `Opening ${service.name}…`;
      if (!popup.document.body) {
        throw new Error("The browser did not initialize the service popup.");
      }
      popup.document.body.textContent = `Opening ${service.name} through BerryKeep…`;
      const launch = await launchClientWebService(service.nodeId, service.id);
      popup.location.replace(launch.url);
    } catch (nextError) {
      popup?.close();
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

      {nodeStates.length > 0 ? (
        <Alert
          color={unavailableNodeCount > 0 ? "yellow" : pendingNodeCount > 0 ? "cyan" : "gray"}
          title={`Checked ${checkedNodeCount} of ${nodeStates.length} node${nodeStates.length === 1 ? "" : "s"}`}
        >
          <Stack gap="xs">
            <Text size="sm">
              {pendingNodeCount > 0
                ? `${pendingNodeCount} node${pendingNodeCount === 1 ? " is" : "s are"} still checking.`
                : `${availableNodeCount} node${availableNodeCount === 1 ? " responded" : "s responded"}.`}
              {unavailableNodeCount > 0
                ? ` ${unavailableNodeCount} node${unavailableNodeCount === 1 ? " could" : "s could"} not be reached.`
                : ""}
            </Text>
            {nodeStates.map((nodeState) => {
              const label =
                nodeState.state === "pending"
                  ? "Checking"
                  : nodeState.state === "unavailable"
                    ? "Unavailable"
                    : nodeState.services.length === 0
                      ? "No services for this device"
                      : `${nodeState.services.length} service${nodeState.services.length === 1 ? "" : "s"} available`;
              const color =
                nodeState.state === "pending"
                  ? "blue"
                  : nodeState.state === "unavailable"
                    ? "yellow"
                    : "green";
              return (
                <Group key={nodeState.nodeId} gap="xs">
                  <Code>{nodeState.nodeId}</Code>
                  <Badge color={color} variant="light">{label}</Badge>
                </Group>
              );
            })}
          </Stack>
        </Alert>
      ) : null}

      {services.length === 0 && nodeStates.length > 0 && checkedNodeCount > 0 ? (
        <Alert
          color={availableNodeCount === 0 && pendingNodeCount === 0 ? "yellow" : "blue"}
          title={
            pendingNodeCount > 0
              ? "No services returned yet"
              : availableNodeCount === 0
                ? "No node could be reached"
                : "No services available to this device"
          }
        >
          {availableNodeCount > 0
            ? `No web service is currently available to this device on ${availableNodeCount} responding node${availableNodeCount === 1 ? "" : "s"}.`
            : "No node has returned an available web-service list yet."}
          {pendingNodeCount > 0
            ? ` ${pendingNodeCount} node${pendingNodeCount === 1 ? " is" : "s are"} still being checked.`
            : availableNodeCount === 0
              ? " Check the connection to the listed nodes and retry."
              : " Configure a service on a node that can reach it and allow this device credential."}
        </Alert>
      ) : null}

      {!loading && services.length === 0 && nodeStates.length === 0 ? (
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
                {embeddedAndroidClient ? (
                  <Button
                    mt="auto"
                    variant="light"
                    loading={launching === launchKey}
                    disabled={launching !== null && launching !== launchKey}
                    onClick={() => void openService(service, "in-app")}
                  >
                    Open in BerryKeep
                  </Button>
                ) : null}
                <Button
                  mt={embeddedAndroidClient ? undefined : "auto"}
                  leftSection={<IconExternalLink size={16} />}
                  loading={launching === launchKey}
                  disabled={launching !== null && launching !== launchKey}
                  onClick={() => void openService(service, "browser")}
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
