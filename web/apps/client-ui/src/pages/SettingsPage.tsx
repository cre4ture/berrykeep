import { Alert, Button, Card, Group, Stack, Text } from "@mantine/core";
import { useClipboard } from "@mantine/hooks";
import { IconCheck, IconCopy } from "@tabler/icons-react";
import {
  exportClientDiagnosticLogs,
  type ClientDiagnosticLogExport,
  type ServerLogEntry
} from "@ironmesh/api";
import { PageHeader } from "@ironmesh/ui";
import { useState } from "react";

const DIAGNOSTIC_LOG_WINDOW_SECS = 3 * 60;

export function SettingsPage() {
  const clipboard = useClipboard({ timeout: 2_000 });
  const [copying, setCopying] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [entryCount, setEntryCount] = useState<number | null>(null);

  async function copyDiagnosticLog() {
    setCopying(true);
    setError(null);

    try {
      const payload = await exportClientDiagnosticLogs(DIAGNOSTIC_LOG_WINDOW_SECS);
      clipboard.copy(formatDiagnosticLogExport(payload));
      setEntryCount(payload.entries.length);
    } catch (nextError) {
      setError(nextError instanceof Error ? nextError.message : "Failed to prepare diagnostic log export");
    } finally {
      setCopying(false);
    }
  }

  return (
    <>
      <PageHeader
        title="Settings"
        description="Client preferences and support diagnostics for this embedded runtime."
      />

      <Stack gap="lg">
        {error ? <Alert color="red" title="Diagnostic log export failed">{error}</Alert> : null}
        <Card withBorder radius="md" padding="lg">
          <Stack gap="sm">
            <Text fw={700}>Diagnostics</Text>
            <Text c="dimmed" size="sm">
              Copy a timestamped snapshot of the most recent three minutes of retained client, SDK, transport, and
              embedded web UI logs. Share this text with support when investigating a problem.
            </Text>
            <Group align="center">
              <Button
                leftSection={clipboard.copied ? <IconCheck size={16} /> : <IconCopy size={16} />}
                loading={copying}
                onClick={() => void copyDiagnosticLog()}
              >
                {clipboard.copied ? "Diagnostic log copied" : "Copy last 3 minutes"}
              </Button>
              {entryCount !== null ? (
                <Text c="dimmed" size="sm">
                  {entryCount} retained {entryCount === 1 ? "entry" : "entries"} exported
                </Text>
              ) : null}
            </Group>
          </Stack>
        </Card>
      </Stack>
    </>
  );
}

function formatDiagnosticLogExport(payload: ClientDiagnosticLogExport): string {
  const generatedAt = formatUnixTimestamp(payload.generated_at_unix);
  const header = [
    "Ironmesh client diagnostic log export",
    `Generated at: ${generatedAt}`,
    `Requested window: last ${payload.requested_window_secs} seconds`,
    `Retained entries: ${payload.entries.length}`,
    ""
  ];

  return [...header, ...payload.entries.flatMap(formatLogEntry)].join("\n");
}

function formatLogEntry(entry: ServerLogEntry): string[] {
  const timestamp = formatUnixTimestamp(entry.captured_at_unix);
  const lines = entry.line.replace(/\r\n?/g, "\n").split("\n");

  while (lines.length > 1 && lines[lines.length - 1] === "") {
    lines.pop();
  }

  return lines.map((line) => (line.length > 0 ? `${timestamp} ${line}` : timestamp));
}

function formatUnixTimestamp(unixTimestamp: number): string {
  if (!Number.isFinite(unixTimestamp) || unixTimestamp <= 0) {
    return "unknown";
  }

  return new Date(unixTimestamp * 1_000).toISOString();
}
