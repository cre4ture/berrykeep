import { JsonBlock } from "@ironmesh/ui";
import { Anchor, Card, List, Stack, Switch, Text } from "@mantine/core";

const TELEMETRY_STRATEGY_DOC_URL =
  "https://github.com/cre4ture/berrykeep/blob/main/docs/server-node-hardware-reliability-telemetry-strategy.md";

// A static, representative shape of the schema-v1 payload this node would send (see
// `docs/server-node-hardware-reliability-telemetry-strategy.md` Section 7). This is intentionally
// NOT a live preview: at first-run setup time, the node-local hardware-health collector has
// typically not completed its first pass yet, so there is no real report to project a payload
// from (see `ReliabilityTelemetryPreviewResponse`'s `unavailable_reason` for the same situation
// post-setup). Showing the field *shape* here, rather than nothing, still satisfies the "operator
// sees what would be sent" goal from Section 3.3/4.4 for this earlier point in the boot sequence.
// After setup, the Hardware settings page's "Payload preview" shows the exact live JSON.
const REPRESENTATIVE_PAYLOAD_EXAMPLE = {
  schema_version: 1,
  telemetry_subject_id: "<generated locally after setup, never this node's real identity>",
  generated_at_unix: 1752912000,
  ironmesh_version: "1.0.38",
  hardware_profile_id: "hp-<hash of normalized hardware inventory>",
  node_lifecycle: {
    uptime_seconds: 431200,
    cumulative_observed_uptime_seconds: 9871200
  },
  storage_devices: [
    {
      component_instance_id: "ci-<hashed component id>",
      is_rotational: false,
      interface_type: "nvme",
      smart: {
        smart_passed: true,
        power_on_hours: 5011,
        reallocated_sector_count: 0,
        media_errors: 0,
        percentage_used: 12
      }
    }
  ],
  memory_ecc: {
    available: true,
    correctable_error_count: 0,
    uncorrectable_error_count: 0
  },
  reliability_findings_summary: [{ finding_code: "chunk_hash_mismatch", occurrence_count: 0 }],
  collectors: [
    { collector_id: "smartctl", available: true },
    { collector_id: "edac", available: true }
  ]
};

type SetupTelemetryDisclosureProps = {
  enabled: boolean;
  onChange: (enabled: boolean) => void;
};

/**
 * Mandatory-but-preselected disclosure step for the first-run setup wizard (doc Section 4.4).
 * Rendered unconditionally as part of both the "start a new cluster" and "join an existing
 * cluster" cards, directly above their completing action, so the operator cannot reach that
 * action without this content having rendered on the page first. The toggle defaults to
 * pre-checked "on" so the outcome remains opt-out even if the operator never touches it.
 */
export function SetupTelemetryDisclosure({ enabled, onChange }: SetupTelemetryDisclosureProps) {
  return (
    <Card withBorder radius="md" padding="md" bg="var(--mantine-color-body)">
      <Stack gap="sm">
        <Text fw={600} size="sm">
          Fleet reliability telemetry
        </Text>
        <Text size="sm" c="dimmed">
          Once set up, this node can periodically send a small, pseudonymized summary of hardware
          reliability signals to the IronMesh project&apos;s central collector, so hardware/firmware
          models with above-average failure rates can be spotted across the whole fleet. This is
          enabled by default (opt-out) — review what that means below before continuing.
        </Text>
        <List size="sm" c="dimmed" spacing={2}>
          <List.Item>
            A pseudonymous subject ID (derived locally, never this node&apos;s real identity,
            cluster ID, or admin labels)
          </List.Item>
          <List.Item>
            A hashed hardware-profile ID, uptime, and per-storage-device SMART health fields (power-on
            hours, reallocated/media-error counts, wear percentage, pass/fail)
          </List.Item>
          <List.Item>RAM ECC error counts (when the board supports it) and thermal-throttle status</List.Item>
          <List.Item>
            A country code, derived server-side from the request&apos;s source IP — never self-reported
            by this node
          </List.Item>
          <List.Item>Counts of internal reliability findings (e.g. checksum mismatches)</List.Item>
          <List.Item>
            Never included: hostnames, IP/MAC addresses, file paths, raw serial numbers, or any
            cluster/operator-identifying label
          </List.Item>
        </List>
        <details>
          <summary style={{ cursor: "pointer", fontSize: "var(--mantine-font-size-sm)" }}>
            Representative payload shape (example values, not a live preview)
          </summary>
          <Text size="xs" c="dimmed" mt={4} mb={4}>
            The real hardware-health report has not been collected yet this early in setup, so this
            shows the field shape this node would send once it has — not the exact next payload.
            After setup, the Hardware settings page shows the exact live JSON before every send.
          </Text>
          <JsonBlock value={REPRESENTATIVE_PAYLOAD_EXAMPLE} />
        </details>
        <Switch
          label="Send anonymized fleet reliability telemetry"
          checked={enabled}
          onChange={(event) => onChange(event.currentTarget.checked)}
        />
        <Text size="xs" c="dimmed">
          You can change this anytime after setup in the Hardware page&apos;s telemetry settings, where
          you can also inspect the exact next payload before it is sent. Full details:{" "}
          <Anchor href={TELEMETRY_STRATEGY_DOC_URL} target="_blank" rel="noreferrer">
            server-node-hardware-reliability-telemetry-strategy.md
          </Anchor>
          .
        </Text>
      </Stack>
    </Card>
  );
}
