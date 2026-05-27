import {
  ActionIcon,
  Box,
  Group,
  Stack,
  Text,
  Tooltip as MantineTooltip,
  VisuallyHidden
} from "@mantine/core";
import { IconZoomIn, IconZoomOut, IconZoomReset } from "@tabler/icons-react";
import { useMemo, useState, type ReactNode } from "react";
import { Brush, ResponsiveContainer } from "recharts";

export type ZoomableTimeSeriesPoint = {
  collectedAtMs: number;
  collectedAtUnix: number;
};

export type ZoomableTimeSeriesBrushRange = {
  startIndex: number;
  endIndex: number;
};

type ZoomableTimeSeriesChartProps<TPoint extends ZoomableTimeSeriesPoint> = {
  points: TPoint[];
  legend?: ReactNode;
  emptyState?: ReactNode;
  height?: string;
  minHeight?: string;
  zoomInAriaLabel: string;
  zoomOutAriaLabel: string;
  resetZoomAriaLabel: string;
  zoomInTooltipLabel?: string;
  zoomOutTooltipLabel?: string;
  resetZoomTooltipLabel?: string;
  renderChart: (props: ZoomableTimeSeriesChartRenderProps<TPoint>) => ReactNode;
};

export type ZoomableTimeSeriesChartRenderProps<TPoint extends ZoomableTimeSeriesPoint> = {
  points: TPoint[];
  xDomain: [number, number];
  totalTimeSpanSeconds: number;
  visibleTimeSpanSeconds: number;
  brush: ReactNode;
};

export function ZoomableTimeSeriesChart<TPoint extends ZoomableTimeSeriesPoint>({
  points,
  legend,
  emptyState,
  height = "19rem",
  minHeight = "19rem",
  zoomInAriaLabel,
  zoomOutAriaLabel,
  resetZoomAriaLabel,
  zoomInTooltipLabel = "Zoom in",
  zoomOutTooltipLabel = "Zoom out",
  resetZoomTooltipLabel = "Reset zoom",
  renderChart
}: ZoomableTimeSeriesChartProps<TPoint>) {
  const [brushRange, setBrushRange] = useState<ZoomableTimeSeriesBrushRange | null>(null);
  const totalTimeSpanSeconds = useMemo(
    () =>
      Math.max(
        0,
        (points[points.length - 1]?.collectedAtUnix ?? 0) - (points[0]?.collectedAtUnix ?? 0)
      ),
    [points]
  );

  if (points.length === 0) {
    return emptyState ?? <Text c="dimmed">No chart samples collected yet.</Text>;
  }

  const resolvedBrushRange = resolveBrushRange(brushRange, points.length);
  const xDomain = buildXDomain(points, resolvedBrushRange);
  const visibleTimeSpanSeconds = Math.max(0, Math.floor((xDomain[1] - xDomain[0]) / 1000));
  const zoomed =
    resolvedBrushRange.startIndex > 0 || resolvedBrushRange.endIndex < points.length - 1;
  const canZoom = points.length > 2;
  const visiblePointCount = resolvedBrushRange.endIndex - resolvedBrushRange.startIndex + 1;

  const handleBrushChange = (nextRange: Partial<ZoomableTimeSeriesBrushRange>) => {
    const nextBrushRange = resolveBrushRange(nextRange, points.length);
    const nextZoomed =
      nextBrushRange.startIndex > 0 || nextBrushRange.endIndex < points.length - 1;
    if (zoomed && !nextZoomed) {
      return;
    }

    setBrushRange(nextBrushRange);
  };

  const setZoomWindow = (visibleRatio: number) => {
    if (!canZoom) {
      return;
    }

    const nextVisiblePointCount = Math.min(
      points.length,
      Math.max(2, Math.round(visiblePointCount * visibleRatio))
    );
    if (nextVisiblePointCount === visiblePointCount) {
      return;
    }

    const centerIndex = (resolvedBrushRange.startIndex + resolvedBrushRange.endIndex) / 2;
    const nextStartIndex = clampBrushStart(
      Math.round(centerIndex - (nextVisiblePointCount - 1) / 2),
      nextVisiblePointCount,
      points.length
    );

    setBrushRange({
      startIndex: nextStartIndex,
      endIndex: nextStartIndex + nextVisiblePointCount - 1
    });
  };

  const brush =
    points.length > 1 ? (
      <Brush
        dataKey="collectedAtMs"
        height={28}
        travellerWidth={8}
        startIndex={resolvedBrushRange.startIndex}
        endIndex={resolvedBrushRange.endIndex}
        onChange={handleBrushChange}
        stroke="#64748b"
        fill="#111827"
        fontSize="0.65rem"
        tickFormatter={(value: number | string) =>
          formatTimeSeriesChartTimestamp(Math.floor(Number(value) / 1000), totalTimeSpanSeconds)
        }
      />
    ) : null;

  return (
    <Stack gap="xs">
      <Box
        style={{
          width: "100%",
          height,
          minHeight,
          borderRadius: "var(--mantine-radius-md)",
          background: "#0f172a",
          padding: "0.75rem 0.5rem 0.25rem"
        }}
      >
        <ResponsiveContainer width="100%" height="100%">
          {renderChart({
            points,
            xDomain,
            totalTimeSpanSeconds,
            visibleTimeSpanSeconds,
            brush
          })}
        </ResponsiveContainer>
      </Box>
      <Group justify="space-between" gap="xs">
        <Box>{legend}</Box>
        <Group gap={4}>
          <MantineTooltip label={zoomInTooltipLabel}>
            <ActionIcon
              aria-label={zoomInAriaLabel}
              disabled={!canZoom || visiblePointCount <= 2}
              size="sm"
              variant="default"
              onClick={() => setZoomWindow(0.5)}
            >
              <IconZoomIn size={16} />
              <VisuallyHidden>{zoomInAriaLabel}</VisuallyHidden>
            </ActionIcon>
          </MantineTooltip>
          <MantineTooltip label={zoomOutTooltipLabel}>
            <ActionIcon
              aria-label={zoomOutAriaLabel}
              disabled={!canZoom || !zoomed}
              size="sm"
              variant="default"
              onClick={() => setZoomWindow(2)}
            >
              <IconZoomOut size={16} />
              <VisuallyHidden>{zoomOutAriaLabel}</VisuallyHidden>
            </ActionIcon>
          </MantineTooltip>
          <MantineTooltip label={resetZoomTooltipLabel}>
            <ActionIcon
              aria-label={resetZoomAriaLabel}
              disabled={!zoomed}
              size="sm"
              variant="default"
              onClick={() => setBrushRange(null)}
            >
              <IconZoomReset size={16} />
              <VisuallyHidden>{resetZoomAriaLabel}</VisuallyHidden>
            </ActionIcon>
          </MantineTooltip>
        </Group>
      </Group>
    </Stack>
  );
}

function resolveBrushRange(
  range: Partial<ZoomableTimeSeriesBrushRange> | null | undefined,
  pointCount: number
): ZoomableTimeSeriesBrushRange {
  const lastIndex = Math.max(0, pointCount - 1);
  const rawStartIndex = Number.isFinite(range?.startIndex) ? Number(range?.startIndex) : 0;
  const rawEndIndex = Number.isFinite(range?.endIndex) ? Number(range?.endIndex) : lastIndex;
  const startIndex = Math.max(0, Math.min(Math.floor(rawStartIndex), lastIndex));
  const endIndex = Math.max(startIndex, Math.min(Math.floor(rawEndIndex), lastIndex));

  return { startIndex, endIndex };
}

function clampBrushStart(startIndex: number, visiblePointCount: number, pointCount: number): number {
  const maxStartIndex = Math.max(0, pointCount - visiblePointCount);
  return Math.max(0, Math.min(startIndex, maxStartIndex));
}

function buildXDomain<TPoint extends ZoomableTimeSeriesPoint>(
  points: TPoint[],
  brushRange: ZoomableTimeSeriesBrushRange
): [number, number] {
  const firstPointMs = points[0]?.collectedAtMs ?? 0;
  if (points.length <= 1) {
    return [firstPointMs - 60_000, firstPointMs + 60_000];
  }

  const startMs = points[brushRange.startIndex]?.collectedAtMs ?? firstPointMs;
  const endMs =
    points[brushRange.endIndex]?.collectedAtMs ?? points[points.length - 1].collectedAtMs;

  if (startMs === endMs) {
    return [startMs - 60_000, endMs + 60_000];
  }

  return [startMs, endMs];
}

export function formatTimeSeriesChartTimestamp(
  unixTs: number | null | undefined,
  timeSpanSeconds: number
): string {
  if (!unixTs || !Number.isFinite(unixTs) || unixTs <= 0) {
    return "unknown";
  }

  const iso = new Date(unixTs * 1000).toISOString();
  if (timeSpanSeconds >= 365 * 24 * 60 * 60) {
    return iso.slice(0, 10);
  }
  if (timeSpanSeconds >= 30 * 24 * 60 * 60) {
    return iso.slice(5, 10);
  }
  if (timeSpanSeconds >= 86_400) {
    return iso.slice(5, 16).replace("T", " ");
  }

  return iso.slice(11, 16);
}
