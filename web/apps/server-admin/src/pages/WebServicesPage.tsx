import {
  createAdminWebService,
  deleteAdminWebService,
  listAdminWebServices,
  listClientCredentials,
  updateAdminWebService,
  type AdminWebService,
  type AdminWebServiceUpsertRequest,
  type ClientCredentialView
} from "@ironmesh/api";
import { ironmeshPrimaryColor } from "@ironmesh/ui";
import {
  Alert,
  Badge,
  Button,
  Card,
  Checkbox,
  Code,
  Grid,
  Group,
  MultiSelect,
  Select,
  Stack,
  Table,
  Text,
  TextInput,
  Textarea
} from "@mantine/core";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useAdminAccess } from "../lib/admin-access";

type TrustMode = "system" | "custom_ca" | "certificate_pin";

type ServiceForm = {
  id: string;
  name: string;
  description: string;
  upstreamUrl: string;
  allowedDeviceIds: string[];
  enabled: boolean;
  trustMode: TrustMode;
  tlsCaPem: string;
  tlsCertificateSha256: string;
  tlsServerName: string;
};

const EMPTY_FORM: ServiceForm = {
  id: "",
  name: "",
  description: "",
  upstreamUrl: "https://",
  allowedDeviceIds: [],
  enabled: true,
  trustMode: "system",
  tlsCaPem: "",
  tlsCertificateSha256: "",
  tlsServerName: ""
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function trustMode(service: AdminWebService): TrustMode {
  if (service.tls_certificate_sha256?.trim()) {
    return "certificate_pin";
  }
  if (service.tls_ca_pem?.trim()) {
    return "custom_ca";
  }
  return "system";
}

function formFromService(service: AdminWebService): ServiceForm {
  return {
    id: service.id,
    name: service.name,
    description: service.description ?? "",
    upstreamUrl: service.upstream_url,
    allowedDeviceIds: service.allowed_device_ids,
    enabled: service.enabled,
    trustMode: trustMode(service),
    tlsCaPem: service.tls_ca_pem ?? "",
    tlsCertificateSha256: service.tls_certificate_sha256 ?? "",
    tlsServerName: service.tls_server_name ?? ""
  };
}

function requestFromForm(form: ServiceForm): AdminWebServiceUpsertRequest {
  const usesTls = form.upstreamUrl.trim().toLowerCase().startsWith("https://");
  return {
    id: form.id.trim(),
    name: form.name.trim(),
    description: form.description.trim() || null,
    upstream_url: form.upstreamUrl.trim(),
    allowed_device_ids: form.allowedDeviceIds,
    enabled: form.enabled,
    tls_ca_pem:
      usesTls && form.trustMode === "custom_ca" ? form.tlsCaPem.trim() || null : null,
    tls_certificate_sha256:
      usesTls && form.trustMode === "certificate_pin"
        ? form.tlsCertificateSha256.trim() || null
        : null,
    tls_server_name: usesTls ? form.tlsServerName.trim() || null : null
  };
}

export function WebServicesPage() {
  const { adminTokenOverride } = useAdminAccess();
  const [services, setServices] = useState<AdminWebService[]>([]);
  const [credentials, setCredentials] = useState<ClientCredentialView[]>([]);
  const [form, setForm] = useState<ServiceForm>(EMPTY_FORM);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [nextServices, nextCredentials] = await Promise.all([
        listAdminWebServices(adminTokenOverride),
        listClientCredentials(adminTokenOverride)
      ]);
      setServices(nextServices);
      setCredentials(nextCredentials);
    } catch (nextError) {
      setError(errorMessage(nextError));
    } finally {
      setLoading(false);
    }
  }, [adminTokenOverride]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const credentialOptions = useMemo(
    () =>
      credentials
        .filter((credential) => credential.revoked_at_unix == null)
        .map((credential) => ({
          value: credential.device_id,
          label: credential.label
            ? `${credential.label} (${credential.device_id})`
            : credential.device_id
        })),
    [credentials]
  );
  const usesTls = form.upstreamUrl.trim().toLowerCase().startsWith("https://");

  function resetForm() {
    setEditingId(null);
    setForm(EMPTY_FORM);
  }

  async function saveService() {
    setSaving(true);
    setError(null);
    try {
      const request = requestFromForm(form);
      if (editingId) {
        await updateAdminWebService(editingId, request, adminTokenOverride);
      } else {
        await createAdminWebService(request, adminTokenOverride);
      }
      resetForm();
      await refresh();
    } catch (nextError) {
      setError(errorMessage(nextError));
    } finally {
      setSaving(false);
    }
  }

  async function removeService(service: AdminWebService) {
    if (!window.confirm(`Delete the private web service ${service.name}?`)) {
      return;
    }
    setError(null);
    try {
      await deleteAdminWebService(service.id, adminTokenOverride);
      if (editingId === service.id) {
        resetForm();
      }
      await refresh();
    } catch (nextError) {
      setError(errorMessage(nextError));
    }
  }

  return (
    <Stack gap="lg">
      {error ? <Alert color="red" title="Web service request failed">{error}</Alert> : null}
      <Alert color="blue" title="Node-local reachability and trust">
        The target is opened by this node, not by the browser. Use system trust for a normal
        certificate, paste an existing CA certificate, or pin the exact SHA-256 fingerprint of an
        existing self-signed certificate. Certificate validation is never globally disabled.
      </Alert>

      <Group justify="space-between" align="flex-start">
        <Text c="dimmed" maw={820}>
          Clients receive only the opaque service ID. The destination URL and TLS settings stay on
          this node, and every connection is checked against the selected device credentials.
        </Text>
        <Button variant="light" loading={loading} onClick={() => void refresh()}>Refresh</Button>
      </Group>

      <Grid>
        <Grid.Col span={{ base: 12, xl: 5 }}>
          <Card withBorder radius="md" padding="lg">
            <Stack gap="md">
              <Group justify="space-between">
                <Text fw={700}>{editingId ? "Edit web service" : "Add web service"}</Text>
                {editingId ? <Badge variant="light">editing</Badge> : null}
              </Group>
              <TextInput
                label="Service ID"
                description="Lowercase DNS label, for example home-nas"
                placeholder="home-nas"
                value={form.id}
                disabled={editingId !== null}
                onChange={(event) => setForm({ ...form, id: event.currentTarget.value })}
                required
              />
              <TextInput
                label="Display name"
                placeholder="Home NAS"
                value={form.name}
                onChange={(event) => setForm({ ...form, name: event.currentTarget.value })}
                required
              />
              <Textarea
                label="Description"
                value={form.description}
                onChange={(event) => setForm({ ...form, description: event.currentTarget.value })}
                autosize
                minRows={2}
              />
              <TextInput
                label="Upstream URL"
                description="Fixed HTTP(S) origin and optional base path reachable from this node"
                placeholder="https://nas.home.arpa:8443/"
                value={form.upstreamUrl}
                onChange={(event) => setForm({ ...form, upstreamUrl: event.currentTarget.value })}
                required
              />
              <MultiSelect
                label="Allowed devices"
                description="Default deny: at least one enrolled device must be selected to expose the service"
                data={credentialOptions}
                searchable
                value={form.allowedDeviceIds}
                onChange={(allowedDeviceIds) => setForm({ ...form, allowedDeviceIds })}
              />
              <Select
                label="HTTPS certificate trust"
                disabled={!usesTls}
                data={[
                  { value: "system", label: "System trust store" },
                  { value: "custom_ca", label: "Existing CA certificate (PEM)" },
                  { value: "certificate_pin", label: "Exact certificate SHA-256 pin" }
                ]}
                value={form.trustMode}
                onChange={(value) =>
                  setForm({ ...form, trustMode: (value as TrustMode | null) ?? "system" })
                }
              />
              {usesTls && form.trustMode === "custom_ca" ? (
                <Textarea
                  label="CA certificate PEM"
                  placeholder="-----BEGIN CERTIFICATE-----"
                  value={form.tlsCaPem}
                  onChange={(event) => setForm({ ...form, tlsCaPem: event.currentTarget.value })}
                  autosize
                  minRows={6}
                  required
                />
              ) : null}
              {usesTls && form.trustMode === "certificate_pin" ? (
                <TextInput
                  label="Certificate SHA-256 fingerprint"
                  description="64 hexadecimal characters; colons are accepted"
                  value={form.tlsCertificateSha256}
                  onChange={(event) =>
                    setForm({ ...form, tlsCertificateSha256: event.currentTarget.value })
                  }
                  required
                />
              ) : null}
              {usesTls ? (
                <TextInput
                  label="TLS server name override"
                  description={
                    form.trustMode === "certificate_pin"
                      ? "Optional SNI name when the URL connects by IP; identity remains bound to the exact pin"
                      : "Optional SNI and certificate name to verify when the URL connects by IP"
                  }
                  placeholder="nas.home.arpa"
                  value={form.tlsServerName}
                  onChange={(event) =>
                    setForm({ ...form, tlsServerName: event.currentTarget.value })
                  }
                />
              ) : null}
              <Checkbox
                label="Service enabled"
                checked={form.enabled}
                onChange={(event) => setForm({ ...form, enabled: event.currentTarget.checked })}
              />
              <Group justify="flex-end">
                {editingId ? <Button variant="default" onClick={resetForm}>Cancel</Button> : null}
                <Button
                  loading={saving}
                  disabled={!form.id.trim() || !form.name.trim() || !form.upstreamUrl.trim()}
                  onClick={() => void saveService()}
                >
                  {editingId ? "Save changes" : "Add service"}
                </Button>
              </Group>
            </Stack>
          </Card>
        </Grid.Col>

        <Grid.Col span={{ base: 12, xl: 7 }}>
          <Card withBorder radius="md" padding="lg">
            <Stack gap="md">
              <Group justify="space-between">
                <Text fw={700}>Configured on this node</Text>
                <Badge color={ironmeshPrimaryColor} variant="light">{services.length}</Badge>
              </Group>
              <Table striped highlightOnHover withTableBorder>
                <Table.Thead>
                  <Table.Tr>
                    <Table.Th>Service</Table.Th>
                    <Table.Th>Upstream</Table.Th>
                    <Table.Th>Trust</Table.Th>
                    <Table.Th>Devices</Table.Th>
                    <Table.Th />
                  </Table.Tr>
                </Table.Thead>
                <Table.Tbody>
                  {services.length ? services.map((service) => (
                    <Table.Tr key={service.id}>
                      <Table.Td>
                        <Text fw={600}>{service.name}</Text>
                        <Group gap="xs">
                          <Code>{service.id}</Code>
                          <Badge color={service.enabled ? ironmeshPrimaryColor : "gray"} variant="light">
                            {service.enabled ? "enabled" : "disabled"}
                          </Badge>
                        </Group>
                      </Table.Td>
                      <Table.Td><Code>{service.upstream_url}</Code></Table.Td>
                      <Table.Td>{trustMode(service).replace("_", " ")}</Table.Td>
                      <Table.Td>{service.allowed_device_ids.length}</Table.Td>
                      <Table.Td>
                        <Group gap="xs" justify="flex-end" wrap="nowrap">
                          <Button
                            size="xs"
                            variant="light"
                            onClick={() => {
                              setEditingId(service.id);
                              setForm(formFromService(service));
                            }}
                          >
                            Edit
                          </Button>
                          <Button size="xs" variant="light" color="red" onClick={() => void removeService(service)}>
                            Delete
                          </Button>
                        </Group>
                      </Table.Td>
                    </Table.Tr>
                  )) : (
                    <Table.Tr>
                      <Table.Td colSpan={5}>
                        <Text c="dimmed">No private web services are configured on this node.</Text>
                      </Table.Td>
                    </Table.Tr>
                  )}
                </Table.Tbody>
              </Table>
            </Stack>
          </Card>
        </Grid.Col>
      </Grid>
    </Stack>
  );
}
