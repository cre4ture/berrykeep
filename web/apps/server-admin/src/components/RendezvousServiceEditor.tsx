import type { RendezvousServiceConfig } from "@berrykeep/api";
import { Button, Checkbox, Fieldset, Group, Stack, TextInput } from "@mantine/core";

type Props = {
  services: RendezvousServiceConfig[];
  onChange: (services: RendezvousServiceConfig[]) => void;
  label: string;
  addLabel: string;
};

export function servicesFromUrls(
  urls: string[],
  services?: RendezvousServiceConfig[]
): RendezvousServiceConfig[] {
  return services ?? urls.map((url) => ({ url, global_registration_enabled: false }));
}

export function RendezvousServiceEditor({ services, onChange, label, addLabel }: Props) {
  function update(index: number, patch: Partial<RendezvousServiceConfig>) {
    onChange(services.map((service, i) => i === index ? { ...service, ...patch } : service));
  }

  return (
    <Stack gap="sm">
      {services.map((service, index) => (
        <Fieldset key={index} legend={`${label} service ${index + 1}`}>
          <Stack gap="sm">
            <Group align="end" wrap="nowrap">
              <TextInput
                label={`${label} URL ${index + 1}`}
                value={service.url}
                onChange={(event) => update(index, { url: event.currentTarget.value })}
                placeholder="https://rendezvous.example:9443"
                style={{ flex: 1, minWidth: 0 }}
              />
              <Button variant="subtle" color="red" aria-label={`Remove ${label} service ${index + 1}`}
                onClick={() => onChange(services.filter((_, i) => i !== index))}>
                Remove
              </Button>
            </Group>
            <Checkbox
              label="Global rendezvous service"
              description="Register this cluster with this service before connecting. Requires HTTPS and a cluster signing node."
              checked={service.global_registration_enabled}
              onChange={(event) => update(index, { global_registration_enabled: event.currentTarget.checked })}
            />
          </Stack>
        </Fieldset>
      ))}
      <Button variant="light" onClick={() => onChange([...services, { url: "", global_registration_enabled: false }])}>
        {addLabel}
      </Button>
    </Stack>
  );
}
